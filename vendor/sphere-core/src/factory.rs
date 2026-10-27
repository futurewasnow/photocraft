//! Factory-calibrated unified (Mei) fisheye projection, in linear premultiplied RGBA.
//! Uses a rotation-only distant-scene model; lens translation/parallax is not solved.
use crate::parallel::*;
use crate::{Error, Result, direction, pixel_count, pixels, rotate, stitch::Stitched};
use serde::{Deserialize, Serialize};
pub mod balance;
pub mod overlap;
pub mod warp;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeiLens {
    pub width: u32,
    pub height: u32,
    /// Converts the suite's (+Y up) world direction to (+Y down) lens coordinates.
    pub world_to_lens: [[f64; 3]; 3],
    pub xi: f64,
    pub focal: [f64; 2],
    /// Continuous coordinates from the image corner, not pixel indices.
    pub center: [f64; 2],
    pub radial: [f64; 3],
    pub tangential: [f64; 2],
    pub max_theta_degrees: f64,
}
impl MeiLens {
    pub fn validate(&self) -> Result<()> {
        pixel_count(self.width, self.height)?;
        if !self.xi.is_finite()
            || !(0.0..=10.0).contains(&self.xi)
            || self
                .focal
                .iter()
                .any(|v| !v.is_finite() || *v <= 0.0 || *v > 65536.0)
            || self.center.iter().any(|v| !v.is_finite())
            || !(0.0..self.width as f64).contains(&self.center[0])
            || !(0.0..self.height as f64).contains(&self.center[1])
            || self
                .radial
                .iter()
                .chain(&self.tangential)
                .any(|v| !v.is_finite() || v.abs() > 100.0)
            || !self.max_theta_degrees.is_finite()
            || !(90.0..=120.0).contains(&self.max_theta_degrees)
        {
            return Err(Error::Invalid("invalid factory fisheye intrinsics"));
        }
        let m = self.world_to_lens;
        if m.iter().flatten().any(|v| !v.is_finite()) {
            return Err(Error::Invalid("non-finite factory lens rotation"));
        }
        for i in 0..3 {
            for j in 0..3 {
                let dot: f64 = (0..3).map(|k| m[i][k] * m[j][k]).sum();
                if (dot - f64::from(i == j)).abs() > 1e-6 {
                    return Err(Error::Invalid("factory lens transform must be orthonormal"));
                }
            }
        }
        // The world uses Y-up, lens coordinates Y-down: exactly one reflection.
        let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
        if (det + 1.0).abs() > 1e-6 {
            return Err(Error::Invalid(
                "factory lens transform has wrong handedness",
            ));
        }
        Ok(())
    }
    fn project(&self, world: [f64; 3]) -> Option<(f64, f64, f64)> {
        let d = rotate(self.world_to_lens, world);
        let theta = d[2].clamp(-1.0, 1.0).acos();
        if theta >= self.max_theta_degrees.to_radians() || d[2] + self.xi <= 1e-9 {
            return None;
        }
        let x = d[0] / (d[2] + self.xi);
        let y = d[1] / (d[2] + self.xi);
        let r2 = x * x + y * y;
        let radial = 1.0 + r2 * (self.radial[0] + r2 * (self.radial[1] + r2 * self.radial[2]));
        let [p1, p2] = self.tangential;
        let u = self.focal[0] * (x * radial + 2.0 * p1 * x * y + p2 * (r2 + 2.0 * x * x))
            + self.center[0];
        let v = self.focal[1] * (y * radial + p1 * (r2 + 2.0 * y * y) + 2.0 * p2 * x * y)
            + self.center[1];
        if !u.is_finite()
            || !v.is_finite()
            || !(0.0..self.width as f64).contains(&u)
            || !(0.0..self.height as f64).contains(&v)
        {
            return None;
        }
        Some((u - 0.5, v - 0.5, theta))
    }
}

pub struct Input<'a> {
    pub lens: &'a MeiLens,
    pub pixels: &'a [[f32; 4]],
}

#[derive(Clone, Copy, Default)]
struct Lookup {
    x: f32,
    y: f32,
    weight: f32,
}

/// Reusable fixed dual-lens geometry. Construction uses f64; cached coordinates use f32.
/// A map owns its calibration so callers cannot accidentally substitute different geometry.
pub struct DualMap {
    lenses: [MeiLens; 2],
    width: u32,
    height: u32,
    lookups: Vec<[Lookup; 2]>,
}
impl DualMap {
    pub fn new(lenses: [MeiLens; 2], width: u32, height: u32) -> Result<Self> {
        if u64::from(width) != 2 * u64::from(height) {
            return Err(Error::Invalid("map output must be 2:1"));
        }
        let count = pixel_count(width, height)?;
        if count
            .checked_mul(std::mem::size_of::<[Lookup; 2]>())
            .is_none_or(|n| n > 1024 * 1024 * 1024)
        {
            return Err(Error::TooLarge);
        }
        let mut source_count = 0;
        for lens in &lenses {
            lens.validate()?;
            source_count += pixel_count(lens.width, lens.height)?;
        }
        if source_count > 128 * 1024 * 1024 {
            return Err(Error::TooLarge);
        }
        let mut lookups = Vec::new();
        lookups
            .try_reserve_exact(count)
            .map_err(|_| Error::Allocation)?;
        lookups.resize(count, [Lookup::default(); 2]);
        fill_lookups(&lenses, width, height, &mut lookups);
        Ok(Self {
            lenses,
            width,
            height,
            lookups,
        })
    }
    /// Reproject directly from the lenses into the stabilized view, reusing map storage.
    /// The rotation maps output-view rays to the original calibrated suite frame.
    pub fn set_view_rotation(&mut self, rotation: crate::rotation::Rotation) {
        let m = rotation.matrix();
        let lenses = self.lenses.clone().map(|mut lens| {
            let old = lens.world_to_lens;
            lens.world_to_lens = std::array::from_fn(|i| {
                std::array::from_fn(|j| (0..3).map(|k| old[i][k] * m[k][j]).sum())
            });
            lens
        });
        fill_lookups(&lenses, self.width, self.height, &mut self.lookups);
    }
    /// Compose a camera-space temporal seam field with the stabilized view.
    /// Samples remain in the original lenses; no intermediate panorama is resampled.
    pub fn set_view_warp(
        &mut self,
        rotation: crate::rotation::Rotation,
        warp: &warp::Temporal,
    ) -> Result<usize> {
        if self.lenses != warp.lenses {
            return Err(Error::Invalid("seam warp calibration does not match map"));
        }
        let m = rotation.matrix();
        let counts = self.lookups.par_iter_mut().enumerate().map(|(i, points)| {
            *points = [Lookup::default(); 2];
            let d = rotate(
                m,
                direction(
                    ((i % self.width as usize) as f64 + 0.5) / f64::from(self.width),
                    ((i / self.width as usize) as f64 + 0.5) / f64::from(self.height),
                ),
            );
            let (a, b, changed) = warp.rays(d);
            let rays = [a, b];
            let mut fallback = None;
            let mut total = 0.;
            let mut invalid = 0usize;
            for (j, lens) in self.lenses.iter().enumerate() {
                if let Some((x, y, theta)) = lens.project(d) {
                    let projected = if changed {
                        lens.project(rays[j])
                    } else {
                        Some((x, y, theta))
                    };
                    let Some((x, y, _)) = projected else {
                        invalid += 1;
                        continue;
                    };
                    let t = ((theta.to_degrees() - 85.) / 10.).clamp(0., 1.);
                    let weight = (1. - t * t * (3. - 2. * t)) as f32;
                    points[j] = Lookup {
                        x: x as f32,
                        y: y as f32,
                        weight,
                    };
                    total += weight;
                    if fallback.is_none_or(|(_, best)| theta < best) {
                        fallback = Some((j, theta));
                    }
                }
            }
            if total > 1e-12 {
                for p in points {
                    p.weight /= total;
                }
            } else if let Some((j, _)) = fallback {
                points[j].weight = 1.;
            }
            (usize::from(changed), invalid)
        });
        let counts = crate::parallel::reduce(counts, || (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        if counts.1 != 0 {
            return Err(Error::Invalid("seam warp left calibrated lens coverage"));
        }
        Ok(counts.0)
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn memory_bytes(&self) -> usize {
        self.lookups.len() * std::mem::size_of::<[Lookup; 2]>()
    }
    /// Render into reusable caller storage; returns uncovered pixel count.
    pub fn render(&self, sources: [&[[f32; 4]]; 2], output: &mut [[f32; 4]]) -> Result<usize> {
        self.render_balanced(sources, output, balance::Gains::IDENTITY)
    }
    /// Apply bounded linear RGB gains before blending; alpha is unchanged.
    pub fn render_balanced(
        &self,
        sources: [&[[f32; 4]]; 2],
        output: &mut [[f32; 4]],
        gains: balance::Gains,
    ) -> Result<usize> {
        if output.len() != self.lookups.len() {
            return Err(Error::Invalid("map output length mismatch"));
        }
        for (lens, source) in self.lenses.iter().zip(sources) {
            if source.len() != pixel_count(lens.width, lens.height)? {
                return Err(Error::Invalid("map source length mismatch"));
            }
        }
        let inputs = [
            Input {
                lens: &self.lenses[0],
                pixels: sources[0],
            },
            Input {
                lens: &self.lenses[1],
                pixels: sources[1],
            },
        ];
        Ok(output
            .par_iter_mut()
            .zip(&self.lookups)
            .map(|(out, points)| {
                *out = [0.; 4];
                for (j, (input, point)) in inputs.iter().zip(points).enumerate() {
                    if point.weight > 0. {
                        let pixel = sample(input, f64::from(point.x), f64::from(point.y));
                        for c in 0..3 {
                            out[c] += pixel[c] * gains.rgb()[j][c] * point.weight;
                        }
                        out[3] += pixel[3] * point.weight;
                    }
                }
                usize::from(out[3] <= 1e-12)
            })
            .sum())
    }
}

fn fill_lookups(lenses: &[MeiLens; 2], width: u32, height: u32, lookups: &mut [[Lookup; 2]]) {
    lookups.par_iter_mut().enumerate().for_each(|(i, points)| {
        *points = [Lookup::default(); 2];
        let d = direction(
            ((i % width as usize) as f64 + 0.5) / width as f64,
            ((i / width as usize) as f64 + 0.5) / height as f64,
        );
        let mut fallback = None;
        let mut total = 0.;
        for (j, lens) in lenses.iter().enumerate() {
            if let Some((x, y, theta)) = lens.project(d) {
                let t = ((theta.to_degrees() - 85.) / 10.).clamp(0., 1.);
                let weight = (1. - t * t * (3. - 2. * t)) as f32;
                points[j] = Lookup {
                    x: x as f32,
                    y: y as f32,
                    weight,
                };
                total += weight;
                if fallback.is_none_or(|(_, best)| theta < best) {
                    fallback = Some((j, theta));
                }
            }
        }
        if total > 1e-12 {
            for p in points {
                p.weight /= total;
            }
        } else if let Some((j, _)) = fallback {
            points[j].weight = 1.;
        }
    });
}

fn sample(input: &Input<'_>, x: f64, y: f64) -> [f32; 4] {
    let w = input.lens.width as usize;
    let h = input.lens.height as usize;
    let x = x.clamp(0.0, (w - 1) as f64);
    let y = y.clamp(0.0, (h - 1) as f64);
    let (ix, iy) = (x.floor() as usize, y.floor() as usize);
    let (fx, fy) = ((x - ix as f64) as f32, (y - iy as f64) as f32);
    let rows = [iy, (iy + 1).min(h - 1)];
    let cols = [ix, (ix + 1).min(w - 1)];
    std::array::from_fn(|c| {
        let top = input.pixels[rows[0] * w + cols[0]][c] * (1.0 - fx)
            + input.pixels[rows[0] * w + cols[1]][c] * fx;
        let bottom = input.pixels[rows[1] * w + cols[0]][c] * (1.0 - fx)
            + input.pixels[rows[1] * w + cols[1]][c] * fx;
        top * (1.0 - fy) + bottom * fy
    })
}
/// Fixed geometry and feathering; no optical flow, stabilization or automatic exposure fit.
pub fn stitch(inputs: &[Input<'_>], width: u32, height: u32) -> Result<Stitched> {
    if inputs.is_empty() || inputs.len() > 64 || u64::from(width) != 2 * u64::from(height) {
        return Err(Error::Invalid(
            "factory stitch needs 1..64 lenses and a 2:1 output",
        ));
    }
    let mut total = 0usize;
    for input in inputs {
        input.lens.validate()?;
        let n = pixel_count(input.lens.width, input.lens.height)?;
        if n != input.pixels.len() {
            return Err(Error::Invalid("factory lens buffer length mismatch"));
        }
        total = total.checked_add(n).ok_or(Error::TooLarge)?;
        if total > 128 * 1024 * 1024 {
            return Err(Error::TooLarge);
        }
    }
    let mut output = pixels(width, height)?;
    let uncovered_pixels = output
        .par_iter_mut()
        .enumerate()
        .map(|(i, out)| {
            let d = direction(
                ((i % width as usize) as f64 + 0.5) / width as f64,
                ((i / width as usize) as f64 + 0.5) / height as f64,
            );
            let mut sum = [0.0; 4];
            let mut weight = 0.0f32;
            let mut fallback = None;
            for input in inputs {
                if let Some((x, y, theta)) = input.lens.project(d) {
                    let pixel = sample(input, x, y);
                    if fallback.is_none_or(|(angle, _)| theta < angle) {
                        fallback = Some((theta, pixel));
                    }
                    let t = ((theta.to_degrees() - 85.0) / 10.0).clamp(0.0, 1.0);
                    let w = (1.0 - t * t * (3.0 - 2.0 * t)) as f32;
                    for c in 0..4 {
                        sum[c] += pixel[c] * w;
                    }
                    weight += w;
                }
            }
            if weight > 1e-12 {
                *out = sum.map(|v| v / weight);
            } else if let Some((_, p)) = fallback {
                *out = p;
            }
            usize::from(out[3] <= 1e-12)
        })
        .sum();
    Ok(Stitched {
        pixels: output,
        uncovered_pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lens(back: bool) -> MeiLens {
        MeiLens {
            width: 64,
            height: 64,
            world_to_lens: if back {
                [[-1., 0., 0.], [0., -1., 0.], [0., 0., -1.]]
            } else {
                [[1., 0., 0.], [0., -1., 0.], [0., 0., 1.]]
            },
            xi: 1.0,
            focal: [24.; 2],
            center: [32.; 2],
            radial: [0.; 3],
            tangential: [0.; 2],
            max_theta_degrees: 100.0,
        }
    }
    #[test]
    fn mei_matches_analytic_stereographic_projection() -> Result<()> {
        let l = lens(false);
        l.validate()?;
        let angle = 60f64.to_radians();
        let p = l
            .project([angle.sin(), 0., angle.cos()])
            .ok_or(Error::Invalid("missing projection"))?;
        assert!((p.0 - (31.5 + 24.0 * (angle / 2.).tan())).abs() < 1e-10);
        assert!((p.1 - 31.5).abs() < 1e-10);
        assert!(l.project([0., 0., -1.]).is_none());
        Ok(())
    }
    #[test]
    fn cached_geometry_reuses_buffers_and_preserves_hdr_alpha() -> Result<()> {
        let lenses = [lens(false), lens(true)];
        let map = DualMap::new(lenses.clone(), 64, 32)?;
        let mut output = pixels(64, 32)?;
        for value in [[-0.25, 2.0, 0.125, 0.5], [4.0, -1.0, 0.5, 1.0]] {
            let source = vec![value; 4096];
            assert_eq!(map.render([&source, &source], &mut output)?, 0);
            for p in &output {
                for c in 0..4 {
                    assert!((p[c] - value[c]).abs() < 1e-6);
                }
            }
        }
        assert!(map.render([&[], &[]], &mut output).is_err());
        assert!(DualMap::new(lenses.clone(), 64, 31).is_err());
        assert!(DualMap::new(lenses, 32768, 16384).is_err());
        Ok(())
    }
    #[test]
    fn reconstructs_direction_field_including_wrap_and_poles() -> Result<()> {
        let lenses = [lens(false), lens(true)];
        let buffers: Vec<Vec<[f32; 4]>> = lenses
            .iter()
            .map(|l| {
                (0..4096)
                    .map(|i| {
                        // Independent inverse of xi=1 (stereographic); d=(2x,2y,1-r2)/(1+r2).
                        let x = ((i % 64) as f64 + 0.5 - 32.) / 24.;
                        let y = ((i / 64) as f64 + 0.5 - 32.) / 24.;
                        let r = x * x + y * y;
                        let local = [2. * x / (1. + r), 2. * y / (1. + r), (1. - r) / (1. + r)];
                        let world: [f64; 3] = std::array::from_fn(|j| {
                            (0..3).map(|k| l.world_to_lens[k][j] * local[k]).sum()
                        });
                        [
                            (0.5 + world[0] * 0.25) as f32,
                            (0.5 + world[1] * 0.25) as f32,
                            (0.5 + world[2] * 0.25) as f32,
                            1.,
                        ]
                    })
                    .collect()
            })
            .collect();
        let inputs: Vec<_> = lenses
            .iter()
            .zip(&buffers)
            .map(|(lens, pixels)| Input { lens, pixels })
            .collect();
        let output = stitch(&inputs, 128, 64)?;
        assert_eq!(output.uncovered_pixels, 0);
        let mut map = DualMap::new(lenses.clone(), 128, 64)?;
        let mut cached = pixels(128, 64)?;
        assert_eq!(map.render([&buffers[0], &buffers[1]], &mut cached)?, 0);
        for (a, b) in cached.iter().zip(&output.pixels) {
            for c in 0..4 {
                assert!((a[c] - b[c]).abs() < 1e-6);
            }
        }
        assert!(
            map.render([&buffers[0][..10], &buffers[1]], &mut cached)
                .is_err()
        );
        for (i, p) in output.pixels.iter().enumerate() {
            let d = direction(
                ((i % 128) as f64 + 0.5) / 128.,
                ((i / 128) as f64 + 0.5) / 64.,
            );
            for c in 0..3 {
                assert!((p[c] as f64 - (0.5 + d[c] * 0.25)).abs() < 0.001);
            }
        }
        // Stabilization changes which source rays are sampled, without resampling a panorama.
        // Repeat and return to identity to catch stale weights from the previous map.
        for vector in [[0.3, 0.8, -0.2], [-1.2, 2.8, 0.7], [0., 0., 0.]] {
            let rotation = crate::rotation::Rotation::from_vector(vector)?;
            map.set_view_rotation(rotation);
            assert_eq!(map.render([&buffers[0], &buffers[1]], &mut cached)?, 0);
            for (i, p) in cached.iter().enumerate() {
                let d = rotation.apply(direction(
                    ((i % 128) as f64 + 0.5) / 128.,
                    ((i / 128) as f64 + 0.5) / 64.,
                ));
                for c in 0..3 {
                    assert!((p[c] as f64 - (0.5 + d[c] * 0.25)).abs() < 0.001);
                }
            }
        }
        let mut broken = lens(false);
        broken.world_to_lens[0][0] = 2.;
        assert!(broken.validate().is_err());
        broken = lens(false);
        broken.radial[0] = f64::NAN;
        assert!(broken.validate().is_err());
        assert!(
            stitch(
                &[Input {
                    lens: &lenses[0],
                    pixels: &buffers[0][..100]
                }],
                128,
                64
            )
            .is_err()
        );
        Ok(())
    }
}
