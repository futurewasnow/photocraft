//! Shared, colour-space-neutral spherical processing. Pixels are premultiplied linear RGBA.
//! +Y is up, +Z is forward, longitude increases toward +X. Pixel centres are sampled.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

pub mod factory;
pub mod finish;
pub mod metadata;
mod parallel;
pub mod rotation;
pub mod stitch;
use parallel::*;
use serde::{Deserialize, Serialize};
use std::f64::consts::{PI, TAU};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("spherical processing cancelled")]
    Cancelled,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("image allocation exceeds the 128 megapixel working limit")]
    TooLarge,
    #[error("unable to allocate image storage")]
    Allocation,
}
pub type Result<T> = std::result::Result<T, Error>;

pub fn pixel_count(width: u32, height: u32) -> Result<usize> {
    let count = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 {
        return Err(Error::Invalid("image dimensions must be positive"));
    }
    if count > 128 * 1024 * 1024 {
        return Err(Error::TooLarge);
    }
    usize::try_from(count).map_err(|_| Error::TooLarge)
}

pub fn pixels(width: u32, height: u32) -> Result<Vec<[f32; 4]>> {
    let count = pixel_count(width, height)?;
    let mut out = Vec::new();
    out.try_reserve_exact(count)
        .map_err(|_| Error::Allocation)?;
    out.resize(count, [0.0; 4]);
    Ok(out)
}

/// A validated borrowed full-sphere mono image. Stereo callers must pass each eye separately.
pub struct Panorama<'a> {
    width: u32,
    height: u32,
    data: &'a [[f32; 4]],
}
impl<'a> Panorama<'a> {
    pub fn new(width: u32, height: u32, data: &'a [[f32; 4]]) -> Result<Self> {
        if u64::from(width) != 2 * u64::from(height) {
            return Err(Error::Invalid(
                "a mono full sphere must have a 2:1 aspect ratio",
            ));
        }
        if pixel_count(width, height)? != data.len() {
            return Err(Error::Invalid(
                "pixel buffer length does not match dimensions",
            ));
        }
        Ok(Self {
            width,
            height,
            data,
        })
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }

    // Pole crossing reflects latitude and rotates longitude by half a turn.
    fn texel(&self, x: i64, y: i64) -> [f32; 4] {
        let h = i64::from(self.height);
        let folded = y.rem_euclid(2 * h);
        let (x, y) = if folded >= h {
            (x + i64::from(self.width / 2), 2 * h - 1 - folded)
        } else {
            (x, folded)
        };
        let index = y as usize * self.width as usize + x.rem_euclid(i64::from(self.width)) as usize;
        self.data.get(index).copied().unwrap_or([0.0; 4])
    }
    pub fn sample_uv(&self, u: f64, v: f64) -> Result<[f32; 4]> {
        if !u.is_finite() || !v.is_finite() {
            return Err(Error::Invalid("sample coordinates must be finite"));
        }
        // Bound coordinates before integer conversion, including hostile large finite values.
        Ok(self.sample(u.rem_euclid(1.0), v.rem_euclid(2.0)))
    }
    fn sample(&self, u: f64, v: f64) -> [f32; 4] {
        let x = u * f64::from(self.width) - 0.5;
        let y = v * f64::from(self.height) - 0.5;
        let (ix, iy) = (x.floor() as i64, y.floor() as i64);
        let (fx, fy) = ((x - x.floor()) as f32, (y - y.floor()) as f32);
        let a = self.texel(ix, iy);
        let b = self.texel(ix + 1, iy);
        let c = self.texel(ix, iy + 1);
        let d = self.texel(ix + 1, iy + 1);
        std::array::from_fn(|i| {
            (a[i] * (1.0 - fx) + b[i] * fx) * (1.0 - fy) + (c[i] * (1.0 - fx) + d[i] * fx) * fy
        })
    }
    pub(crate) fn direction(&self, d: [f64; 3]) -> [f32; 4] {
        self.sample(
            (d[0].atan2(d[2]) / TAU + 0.5).rem_euclid(1.0),
            0.5 - d[1].clamp(-1.0, 1.0).asin() / PI,
        )
    }
}

pub fn direction(u: f64, v: f64) -> [f64; 3] {
    let lon = (u - 0.5) * TAU;
    let lat = (0.5 - v) * PI;
    [lat.cos() * lon.sin(), lat.sin(), lat.cos() * lon.cos()]
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Orientation {
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
}
impl Orientation {
    pub fn validate(self) -> Result<Self> {
        if [self.yaw, self.pitch, self.roll]
            .iter()
            .any(|v| !v.is_finite() || v.abs() > 36000.0)
        {
            return Err(Error::Invalid(
                "angles must be finite and within +/-36000 degrees",
            ));
        }
        Ok(self)
    }
    pub(crate) fn matrix(self) -> [[f64; 3]; 3] {
        let (sy, cy) = self.yaw.to_radians().sin_cos();
        let (sp, cp) = self.pitch.to_radians().sin_cos();
        let (sr, cr) = self.roll.to_radians().sin_cos();
        [
            [cy * cr - sy * sp * sr, -cy * sr - sy * sp * cr, sy * cp],
            [cp * sr, cp * cr, sp],
            [-sy * cr - cy * sp * sr, sy * sr - cy * sp * cr, cy * cp],
        ]
    }
}
pub(crate) fn rotate(m: [[f64; 3]; 3], d: [f64; 3]) -> [f64; 3] {
    m.map(|r| r[0] * d[0] + r[1] * d[1] + r[2] * d[2])
}

/// Inverse-map a view orientation: positive yaw looks right, positive pitch looks up.
pub fn orient(source: &Panorama<'_>, orientation: Orientation) -> Result<Vec<[f32; 4]>> {
    let m = orientation.validate()?.matrix();
    let mut out = pixels(source.width, source.height)?;
    out.par_iter_mut().enumerate().for_each(|(i, p)| {
        let d = direction(
            (i % source.width as usize) as f64 / f64::from(source.width)
                + 0.5 / f64::from(source.width),
            (i / source.width as usize) as f64 / f64::from(source.height)
                + 0.5 / f64::from(source.height),
        );
        *p = source.direction(rotate(m, d));
    });
    Ok(out)
}

pub fn perspective(
    source: &Panorama<'_>,
    width: u32,
    height: u32,
    fov: f64,
    orientation: Orientation,
) -> Result<Vec<[f32; 4]>> {
    if !fov.is_finite() || !(1.0..179.0).contains(&fov) {
        return Err(Error::Invalid("horizontal FOV must be in [1,179) degrees"));
    }
    let m = orientation.validate()?.matrix();
    let mut out = pixels(width, height)?;
    let t = (fov.to_radians() / 2.0).tan();
    out.par_iter_mut().enumerate().for_each(|(i, p)| {
        let x = (2.0 * (i % width as usize) as f64 + 1.0) / f64::from(width) - 1.0;
        let y = 1.0 - (2.0 * (i / width as usize) as f64 + 1.0) / f64::from(height);
        let d = [x * t, y * t * f64::from(height) / f64::from(width), 1.0];
        let len = (d[0] * d[0] + d[1] * d[1] + 1.0).sqrt();
        *p = source.direction(rotate(m, d.map(|v| v / len)));
    });
    Ok(out)
}

/// Great-circle distance, not planar distance: a brush crosses the seam and behaves at the poles.
pub fn brush_weight(
    u: f64,
    v: f64,
    center_u: f64,
    center_v: f64,
    radius_degrees: f64,
) -> Result<f32> {
    if [u, v, center_u, center_v, radius_degrees]
        .iter()
        .any(|v| !v.is_finite())
        || !(0.0..=1.0).contains(&v)
        || !(0.0..=1.0).contains(&center_v)
        || !(0.0..=180.0).contains(&radius_degrees)
        || radius_degrees == 0.0
    {
        return Err(Error::Invalid(
            "invalid spherical brush coordinates or radius",
        ));
    }
    let a = direction(u.rem_euclid(1.0), v);
    let b = direction(center_u.rem_euclid(1.0), center_v);
    let angle = (a[0] * b[0] + a[1] * b[1] + a[2] * b[2])
        .clamp(-1.0, 1.0)
        .acos();
    let t = (1.0 - angle / radius_degrees.to_radians()).clamp(0.0, 1.0);
    Ok((t * t * (3.0 - 2.0 * t)) as f32)
}

/// Spherical Gaussian-like blur with fixed tangent-disc quadrature. Angular radius is independent
/// of resolution; neighbours follow great circles and cross the poles and seam correctly.
pub fn blur(source: &Panorama<'_>, radius_degrees: f64) -> Result<Vec<[f32; 4]>> {
    if !radius_degrees.is_finite() || !(0.0..=30.0).contains(&radius_degrees) {
        return Err(Error::Invalid("blur radius must be 0..30 degrees"));
    }
    if radius_degrees == 0.0 {
        return Ok(source.data.to_vec());
    }
    let mut out = pixels(source.width, source.height)?;
    let radius = radius_degrees.to_radians();
    out.par_iter_mut().enumerate().for_each(|(i, p)| {
        let u = (i % source.width as usize) as f64 / f64::from(source.width)
            + 0.5 / f64::from(source.width);
        let v = (i / source.width as usize) as f64 / f64::from(source.height)
            + 0.5 / f64::from(source.height);
        let d = direction(u, v);
        let lon = (u - 0.5) * TAU;
        let east = [lon.cos(), 0.0, -lon.sin()];
        let north = [
            -d[1] * lon.sin(),
            (1.0 - d[1] * d[1]).max(0.0).sqrt(),
            -d[1] * lon.cos(),
        ];
        let mut sum = source.direction(d);
        let mut total = 1.0f32;
        for ring in 1..=3 {
            let r = radius * f64::from(ring) / 3.0;
            let weight = (-2.0 * (f64::from(ring) / 3.0).powi(2)).exp() as f32;
            for k in 0..12 {
                let a = f64::from(k) * TAU / 12.0;
                let q = std::array::from_fn(|j| {
                    d[j] * r.cos() + (east[j] * a.cos() + north[j] * a.sin()) * r.sin()
                });
                let s = source.direction(q);
                for j in 0..4 {
                    sum[j] += s[j] * weight;
                }
                total += weight;
            }
        }
        *p = sum.map(|v| v / total);
    });
    Ok(out)
}
