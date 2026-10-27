//! Calibrated rig stitching. Fixed camera geometry/weights can be reused for each video frame.
//! This stage does not infer camera calibration, synchronize clips or solve parallax.
use crate::parallel::*;
use crate::{Error, Orientation, Result, direction, pixel_count, pixels, rotate};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lens {
    Rectilinear,
    EquidistantFisheye,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Camera {
    pub width: u32,
    pub height: u32,
    pub lens: Lens,
    /// Horizontal field of view; for fisheye, diameter of the circular image.
    pub fov: f64,
    #[serde(default)]
    pub orientation: Orientation,
    /// Normalized centre of the lens in the image.
    #[serde(default = "center")]
    pub center: [f64; 2],
    /// Fisheye image-circle radius in source pixels, measured at half the declared FOV.
    /// Omit to use half the shorter image dimension. Supports offset/cropped image circles.
    #[serde(default)]
    pub radius_pixels: Option<f64>,
    /// Linear-light exposure compensation, in stops.
    #[serde(default)]
    pub exposure: f64,
}
fn center() -> [f64; 2] {
    [0.5, 0.5]
}
impl Camera {
    pub fn validate(&self) -> Result<()> {
        pixel_count(self.width, self.height)?;
        self.orientation.validate()?;
        let max = match self.lens {
            Lens::Rectilinear => 179.0,
            Lens::EquidistantFisheye => 360.0,
        };
        if !self.fov.is_finite() || self.fov < 1.0 || self.fov >= max {
            return Err(Error::Invalid("invalid camera field of view"));
        }
        if self
            .center
            .iter()
            .any(|x| !x.is_finite() || !(0.0..=1.0).contains(x))
        {
            return Err(Error::Invalid("lens centre must be within the image"));
        }
        if !self.exposure.is_finite() || self.exposure.abs() > 16.0 {
            return Err(Error::Invalid("camera exposure must be within +/-16 stops"));
        }
        if let Some(radius) = self.radius_pixels {
            if !matches!(self.lens, Lens::EquidistantFisheye) {
                return Err(Error::Invalid(
                    "image-circle radius only applies to fisheye lenses",
                ));
            }
            if !radius.is_finite()
                || radius < 0.5
                || radius > 2.0 * f64::from(self.width.max(self.height))
            {
                return Err(Error::Invalid(
                    "fisheye radius is outside the supported image bounds",
                ));
            }
        }
        Ok(())
    }
    fn project(&self, d: [f64; 3], matrix: [[f64; 3]; 3]) -> Option<(f64, f64, f32)> {
        // World -> camera uses the transpose of the camera -> world rotation.
        let m = std::array::from_fn(|i| std::array::from_fn(|j| matrix[j][i]));
        let d = rotate(m, d);
        let half = self.fov.to_radians() / 2.0;
        let (x, y, edge) = match self.lens {
            Lens::Rectilinear => {
                if d[2] <= 0.0 {
                    return None;
                }
                let x = d[0] / (d[2] * half.tan());
                let y =
                    -d[1] / (d[2] * half.tan()) * f64::from(self.width) / f64::from(self.height);
                (x, y, 1.0 - x.abs().max(y.abs()))
            }
            Lens::EquidistantFisheye => {
                let theta = d[2].clamp(-1.0, 1.0).acos();
                let r = theta / half;
                let n = d[0].hypot(d[1]);
                let (x, y) = if n < 1e-12 {
                    (0.0, 0.0)
                } else {
                    (r * d[0] / n, -r * d[1] / n)
                };
                let diameter = 2.0
                    * self
                        .radius_pixels
                        .unwrap_or(f64::from(self.width.min(self.height)) / 2.0);
                (
                    x * diameter / f64::from(self.width),
                    y * diameter / f64::from(self.height),
                    1.0 - r,
                )
            }
        };
        let u = self.center[0] + x / 2.0;
        let v = self.center[1] + y / 2.0;
        if edge <= 0.0 || !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
            return None;
        }
        Some((
            u * f64::from(self.width) - 0.5,
            v * f64::from(self.height) - 0.5,
            (edge * edge) as f32,
        ))
    }
}

pub struct Input<'a> {
    pub camera: &'a Camera,
    pub pixels: &'a [[f32; 4]],
}
pub struct Stitched {
    pub pixels: Vec<[f32; 4]>,
    pub uncovered_pixels: usize,
}

pub fn stitch(inputs: &[Input<'_>], width: u32, height: u32) -> Result<Stitched> {
    if u64::from(width) != 2 * u64::from(height) {
        return Err(Error::Invalid("stitch output must be a 2:1 full sphere"));
    }
    if inputs.is_empty() || inputs.len() > 64 {
        return Err(Error::Invalid("a rig requires between 1 and 64 cameras"));
    }
    for input in inputs {
        input.camera.validate()?;
        if input.pixels.len() != pixel_count(input.camera.width, input.camera.height)? {
            return Err(Error::Invalid("camera buffer has incorrect length"));
        }
    }
    let matrices: Vec<_> = inputs
        .iter()
        .map(|i| i.camera.orientation.matrix())
        .collect();
    let gains: Vec<_> = inputs
        .iter()
        .map(|i| 2.0f32.powf(i.camera.exposure as f32))
        .collect();
    let mut out = pixels(width, height)?;
    let uncovered = out
        .par_iter_mut()
        .enumerate()
        .map(|(i, p)| {
            let d = direction(
                ((i % width as usize) as f64 + 0.5) / f64::from(width),
                ((i / width as usize) as f64 + 0.5) / f64::from(height),
            );
            let mut sum = [0.0; 4];
            let mut weight = 0.0;
            for ((input, m), gain) in inputs.iter().zip(&matrices).zip(&gains) {
                if let Some((x, y, w)) = input.camera.project(d, *m) {
                    let s = bilinear(input, x, y);
                    for j in 0..3 {
                        sum[j] += s[j] * w * gain;
                    }
                    sum[3] += s[3] * w;
                    weight += w;
                }
            }
            if weight > 1e-12 && sum[3] > 1e-12 {
                *p = sum.map(|v| v / weight);
                0
            } else {
                1
            }
        })
        .sum();
    Ok(Stitched {
        pixels: out,
        uncovered_pixels: uncovered,
    })
}
fn bilinear(input: &Input<'_>, x: f64, y: f64) -> [f32; 4] {
    let c = input.camera;
    let (ix, iy) = (x.floor() as i64, y.floor() as i64);
    let (fx, fy) = ((x - x.floor()) as f32, (y - y.floor()) as f32);
    let get = |x: i64, y: i64| {
        if x < 0 || y < 0 || x >= i64::from(c.width) || y >= i64::from(c.height) {
            return None;
        }
        if matches!(c.lens, Lens::EquidistantFisheye) {
            let radius = c
                .radius_pixels
                .unwrap_or(f64::from(c.width.min(c.height)) / 2.0);
            let dx = x as f64 + 0.5 - c.center[0] * f64::from(c.width);
            let dy = y as f64 + 0.5 - c.center[1] * f64::from(c.height);
            if dx * dx + dy * dy > radius * radius {
                return None;
            }
        }
        input
            .pixels
            .get(y as usize * c.width as usize + x as usize)
            .copied()
    };
    // Exclude pixels outside the calibrated optical footprint before interpolation.
    // Renormalize geometric weights only; genuine source alpha remains premultiplied.
    let mut sum = [0.0; 4];
    let mut total = 0.0;
    for (xx, yy, weight) in [
        (ix, iy, (1.0 - fx) * (1.0 - fy)),
        (ix + 1, iy, fx * (1.0 - fy)),
        (ix, iy + 1, (1.0 - fx) * fy),
        (ix + 1, iy + 1, fx * fy),
    ] {
        if let Some(pixel) = get(xx, yy) {
            for j in 0..4 {
                sum[j] += pixel[j] * weight;
            }
            total += weight;
        }
    }
    if total > 1e-12 {
        sum.map(|v| v / total)
    } else {
        [0.0; 4]
    }
}
