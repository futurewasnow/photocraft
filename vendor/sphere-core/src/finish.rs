//! Conservative, projection-aware still finishing. No trained model or vendor runtime.
//! Noise estimates are diagnostic, not a guarantee that texture can be distinguished from noise.
use crate::parallel::*;
use crate::{Error, Orientation, Panorama, Result, direction, pixel_count, pixels};
use serde::{Deserialize, Serialize};
use std::f64::consts::{PI, TAU};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DenoiseOptions {
    /// Standard deviation in straight linear RGB. None estimates it from opaque source blocks.
    pub sigma: Option<f32>,
    pub strength: f32,
    /// Angular footprint expressed in equatorial pixels; it remains circular at the poles.
    pub radius_pixels: f64,
}
impl Default for DenoiseOptions {
    fn default() -> Self {
        Self {
            sigma: None,
            strength: 0.8,
            radius_pixels: 1.5,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DenoiseReport {
    pub sigma: f32,
    pub estimated: bool,
    pub sampled_blocks: usize,
    pub applied: bool,
    pub reason: &'static str,
}
pub struct Denoised {
    pub pixels: Vec<[f32; 4]>,
    pub report: DenoiseReport,
}

fn validate_pixels(data: &[[f32; 4]]) -> Result<()> {
    if data
        .par_iter()
        .any(|p| p.iter().any(|v| !v.is_finite() || v.abs() > 1e6) || !(0.0..=1.0).contains(&p[3]))
    {
        return Err(Error::Invalid(
            "finishing requires finite premultiplied pixels and alpha in 0..1",
        ));
    }
    Ok(())
}

/// Robust high-frequency estimate on bounded, opaque 2x2 blocks. Strong edges and clipped
/// blocks are excluded. Detailed textures may still bias the estimate: expose sigma to review.
fn estimate_noise(source: &Panorama<'_>) -> (f32, usize) {
    let columns = source.width as usize / 2;
    let rows = source.height as usize / 2;
    let blocks = columns * rows;
    let stride = blocks.div_ceil(32768).max(1);
    let mut coefficients = Vec::with_capacity(32768 * 3);
    for block in (0..blocks).step_by(stride) {
        let x = (block % columns * 2) as i64;
        let y = (block / columns * 2) as i64;
        let p = [
            source.texel(x, y),
            source.texel(x + 1, y),
            source.texel(x, y + 1),
            source.texel(x + 1, y + 1),
        ];
        if p.iter()
            .any(|v| v[3] < 0.999 || v[..3].iter().any(|c| *c <= 1e-5 || *c >= 64.0))
        {
            continue;
        }
        let mut coeff = [0.0f32; 3];
        let mut edge = false;
        for c in 0..3 {
            let v = p.map(|q| q[c] / q[3]);
            let lo = v.iter().copied().fold(f32::INFINITY, f32::min);
            let hi = v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            edge |= hi - lo > 0.25;
            coeff[c] = ((v[0] - v[1] - v[2] + v[3]) * 0.5).abs();
        }
        if !edge {
            coefficients.extend(coeff);
        }
    }
    let samples = coefficients.len() / 3;
    if samples < 64 {
        return (0.0, samples);
    }
    coefficients.sort_unstable_by(f32::total_cmp);
    (coefficients[coefficients.len() / 2] / 0.674_489_74, samples)
}

/// Edge-aware spherical bilateral filtering, preserving original alpha and source dimensions.
/// This is spatial denoising for stills, not motion-compensated temporal video denoising.
pub fn denoise(source: &Panorama<'_>, options: DenoiseOptions) -> Result<Denoised> {
    denoise_with_control(source, options, &|_| true)
}

/// Returning false from a row-boundary progress callback cancels without changing the source.
pub fn denoise_with_control(
    source: &Panorama<'_>,
    options: DenoiseOptions,
    control: &(impl Fn(f32) -> bool + Sync),
) -> Result<Denoised> {
    if !control(0.0) {
        return Err(Error::Cancelled);
    }
    if !options.strength.is_finite()
        || !(0.0..=1.0).contains(&options.strength)
        || !options.radius_pixels.is_finite()
        || !(0.5..=3.0).contains(&options.radius_pixels)
        || options
            .sigma
            .is_some_and(|s| !s.is_finite() || !(0.0..=0.15).contains(&s))
    {
        return Err(Error::Invalid(
            "denoise requires strength 0..1, radius 0.5..3 pixels, sigma 0..0.15",
        ));
    }
    validate_pixels(source.data)?;
    let (sigma, sampled_blocks) = options
        .sigma
        .map_or_else(|| estimate_noise(source), |s| (s, 0));
    let reason = if options.strength == 0.0 {
        "disabled"
    } else if options.sigma.is_none() && sampled_blocks < 64 {
        "insufficient opaque unclipped samples"
    } else if sigma < 0.0001 {
        "no measurable noise"
    } else if sigma > 0.15 {
        "estimate outside conservative automatic range"
    } else {
        "spatial filtering applied; texture review required"
    };
    let applied = options.strength > 0.0 && (0.0001..=0.15).contains(&sigma);
    let report = DenoiseReport {
        sigma,
        estimated: options.sigma.is_none(),
        sampled_blocks,
        applied,
        reason,
    };
    let mut out = pixels(source.width, source.height)?;
    if !applied {
        if !control(1.0) {
            return Err(Error::Cancelled);
        }
        out.copy_from_slice(source.data);
        return Ok(Denoised {
            pixels: out,
            report,
        });
    }
    let radius = options.radius_pixels * TAU / f64::from(source.width);
    let taps: [(f64, f64, f64, f64); 24] = std::array::from_fn(|i| {
        let ring = (i / 12 + 1) as f64;
        let r = radius * ring / 2.0;
        let a = (i % 12) as f64 * TAU / 12.0;
        (
            r.cos(),
            r.sin() * a.cos(),
            r.sin() * a.sin(),
            (-0.5 * ring * ring).exp(),
        )
    });
    // Longitude rotates d/east/north together. In the longitude-zero frame a tap
    // is (ec, sin(lat)*dc + cos(lat)*nc, cos(lat)*dc - sin(lat)*nc).
    // Its latitude and longitude offset therefore depend only on the source row.
    // Cache those coordinates once, retaining the same tangent disc and bilinear
    // sampling at the wrap and poles instead of evaluating 24 atan2/asin pairs per pixel.
    let mut row_taps = Vec::new();
    row_taps
        .try_reserve_exact(source.height as usize)
        .map_err(|_| Error::Allocation)?;
    for y in 0..source.height {
        if !control(0.05 * y as f32 / source.height as f32) {
            return Err(Error::Cancelled);
        }
        let lat = (0.5 - (f64::from(y) + 0.5) / f64::from(source.height)) * PI;
        let (sl, cl) = lat.sin_cos();
        row_taps.push(taps.map(|(dc, ec, nc, spatial)| {
            (
                ec.atan2(cl * dc - sl * nc) / TAU,
                0.5 - (sl * dc + cl * nc).clamp(-1.0, 1.0).asin() / PI,
                spatial,
            )
        }));
    }
    let range_scale = 1.0 / (2.0 * 3.0 * (2.0 * f64::from(sigma)).powi(2));
    out.par_iter_mut()
        .enumerate()
        .try_for_each(|(i, p)| -> Result<()> {
            if i.is_multiple_of(source.width as usize)
                && !control(0.05 + 0.95 * i as f32 / source.data.len() as f32)
            {
                return Err(Error::Cancelled);
            }
            let center = source.data[i];
            if center[3] < 1e-6 {
                *p = center;
                return Ok(());
            }
            let straight = center.map(|v| f64::from(v) / f64::from(center[3]));
            let u = ((i % source.width as usize) as f64 + 0.5) / f64::from(source.width);
            let mut sum = [straight[0], straight[1], straight[2]];
            let mut total = 1.0;
            for (&(du, v, spatial), &(dc, ec, nc, _)) in
                row_taps[i / source.width as usize].iter().zip(&taps)
            {
                let mut sample = source.sample((u + du).rem_euclid(1.0), v);
                // At an exact pole longitude is undefined, and algebraically
                // equivalent directions can select different source columns. Preserve
                // the original construction there and at hard alpha-gate boundaries.
                // asin near +/-1 magnifies double-precision roundoff to roughly
                // 1e-8 radians; include that numerical pole neighbourhood.
                if v <= 1e-7
                    || v >= 1.0 - 1e-7
                    || ((sample[3] - center[3]).abs() - 0.05).abs() < 8.0 * f32::EPSILON
                    || (sample[3] - 1e-6).abs() < 8.0 * f32::EPSILON * 1e-6
                {
                    let source_v =
                        ((i / source.width as usize) as f64 + 0.5) / f64::from(source.height);
                    let d = direction(u, source_v);
                    let (sl, cl) = ((u - 0.5) * TAU).sin_cos();
                    let east = [cl, 0.0, -sl];
                    let north = [-d[1] * sl, (1.0 - d[1] * d[1]).max(0.0).sqrt(), -d[1] * cl];
                    sample = source.direction(std::array::from_fn(|j| {
                        d[j] * dc + east[j] * ec + north[j] * nc
                    }));
                }
                if sample[3] < 1e-6 || (sample[3] - center[3]).abs() > 0.05 {
                    continue;
                }
                let s = sample.map(|v| f64::from(v) / f64::from(sample[3]));
                let distance = (0..3).map(|c| (s[c] - straight[c]).powi(2)).sum::<f64>();
                let weight = spatial * (-distance * range_scale).exp();
                for c in 0..3 {
                    sum[c] += s[c] * weight;
                }
                total += weight;
            }
            *p = center;
            for c in 0..3 {
                p[c] = ((straight[c] * (1.0 - f64::from(options.strength))
                    + sum[c] / total * f64::from(options.strength))
                    * f64::from(center[3])) as f32;
            }
            Ok(())
        })?;
    if !control(1.0) {
        return Err(Error::Cancelled);
    }
    Ok(Denoised {
        pixels: out,
        report,
    })
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Pole {
    #[default]
    Nadir,
    Zenith,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolePatch {
    pub pole: Pole,
    /// Outer radius of the patch mask, measured on the sphere.
    pub radius_degrees: f64,
    /// Transition lies inside the radius; the inner disc is completely replaced.
    pub feather_degrees: f64,
    pub yaw: f64,
}
impl Default for PolePatch {
    fn default() -> Self {
        Self {
            pole: Pole::Nadir,
            radius_degrees: 20.0,
            feather_degrees: 5.0,
            yaw: 0.0,
        }
    }
}

pub struct Plate<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [[f32; 4]],
    /// Horizontal FOV of the rectilinear clean plate; must match its actual projection.
    pub horizontal_fov: f64,
}

/// Composite a supplied rectilinear clean plate into a pole cap in premultiplied linear light.
/// No invented scene content: the caller supplies and aligns the clean plate. Unmodified pixels
/// are bit-identical. Transparent donors use source-over rather than punching new holes.
pub fn patch_pole(
    source: &Panorama<'_>,
    plate: &Plate<'_>,
    patch: PolePatch,
) -> Result<Vec<[f32; 4]>> {
    patch_pole_with_control(source, plate, patch, &|_| true)
}

/// Cancellable clean-plate compositing, with progress checked at row boundaries.
pub fn patch_pole_with_control(
    source: &Panorama<'_>,
    plate: &Plate<'_>,
    patch: PolePatch,
    control: &(impl Fn(f32) -> bool + Sync),
) -> Result<Vec<[f32; 4]>> {
    if !control(0.0) {
        return Err(Error::Cancelled);
    }
    if pixel_count(plate.width, plate.height)? != plate.pixels.len()
        || plate.width < 2
        || plate.height < 2
    {
        return Err(Error::Invalid(
            "patch plate dimensions do not match its buffer or are below 2 pixels",
        ));
    }
    if !plate.horizontal_fov.is_finite()
        || !(1.0..170.0).contains(&plate.horizontal_fov)
        || !patch.radius_degrees.is_finite()
        || !(0.1..=60.0).contains(&patch.radius_degrees)
        || !patch.feather_degrees.is_finite()
        || patch.feather_degrees <= 0.0
        || patch.feather_degrees > patch.radius_degrees
    {
        return Err(Error::Invalid(
            "invalid patch FOV, radius (0.1..60) or feather (0..radius]",
        ));
    }
    let tangent = (plate.horizontal_fov.to_radians() * 0.5).tan();
    let vertical_tangent = tangent * f64::from(plate.height) / f64::from(plate.width);
    // Require bilinear sample centres, not merely geometric FOV, to cover the whole cap.
    let usable = (tangent * (1.0 - 1.0 / f64::from(plate.width)))
        .min(vertical_tangent * (1.0 - 1.0 / f64::from(plate.height)));
    if patch.radius_degrees.to_radians().tan() >= usable {
        return Err(Error::Invalid(
            "clean plate FOV/resolution does not cover the entire patch cap",
        ));
    }
    let pitch = match patch.pole {
        Pole::Nadir => -90.0,
        Pole::Zenith => 90.0,
    };
    let m = Orientation {
        yaw: patch.yaw,
        pitch,
        roll: 0.0,
    }
    .validate()?
    .matrix();
    validate_pixels(source.data)?;
    validate_pixels(plate.pixels)?;
    let mut out = pixels(source.width, source.height)?;
    let radius = patch.radius_degrees.to_radians();
    let feather = patch.feather_degrees.to_radians();
    out.par_iter_mut()
        .enumerate()
        .try_for_each(|(i, p)| -> Result<()> {
            if i.is_multiple_of(source.width as usize)
                && !control(i as f32 / source.data.len() as f32)
            {
                return Err(Error::Cancelled);
            }
            *p = source.data[i];
            let u = ((i % source.width as usize) as f64 + 0.5) / f64::from(source.width);
            let v = ((i / source.width as usize) as f64 + 0.5) / f64::from(source.height);
            let angle = match patch.pole {
                Pole::Zenith => v * PI,
                Pole::Nadir => (1.0 - v) * PI,
            };
            if angle >= radius {
                return Ok(());
            }
            let d = direction(u, v);
            let local: [f64; 3] =
                std::array::from_fn(|c| m[0][c] * d[0] + m[1][c] * d[1] + m[2][c] * d[2]);
            let x = ((local[0] / local[2] / tangent + 1.0) * 0.5 * f64::from(plate.width) - 0.5)
                .clamp(0.0, f64::from(plate.width - 1));
            let y =
                ((1.0 - local[1] / local[2] / vertical_tangent) * 0.5 * f64::from(plate.height)
                    - 0.5)
                    .clamp(0.0, f64::from(plate.height - 1));
            let (ix, iy) = (x.floor() as usize, y.floor() as usize);
            let (jx, jy) = (
                (ix + 1).min(plate.width as usize - 1),
                (iy + 1).min(plate.height as usize - 1),
            );
            let (fx, fy) = ((x - ix as f64) as f32, (y - iy as f64) as f32);
            let t = ((radius - angle) / feather).clamp(0.0, 1.0);
            let weight = (t * t * (3.0 - 2.0 * t)) as f32;
            let donor: [f32; 4] = std::array::from_fn(|c| {
                ((plate.pixels[iy * plate.width as usize + ix][c] * (1.0 - fx)
                    + plate.pixels[iy * plate.width as usize + jx][c] * fx)
                    * (1.0 - fy)
                    + (plate.pixels[jy * plate.width as usize + ix][c] * (1.0 - fx)
                        + plate.pixels[jy * plate.width as usize + jx][c] * fx)
                        * fy)
                    * weight
            });
            *p = std::array::from_fn(|c| donor[c] + source.data[i][c] * (1.0 - donor[3]));
            Ok(())
        })?;
    if !control(1.0) {
        return Err(Error::Cancelled);
    }
    Ok(out)
}

#[derive(Debug, Default, Serialize)]
pub struct CapCoverage {
    pub sampled_pixels: u64,
    pub missing_pixels: u64,
    pub partial_pixels: u64,
    /// Fraction of spherical cap area, compensating for equirectangular oversampling at poles.
    pub missing_area_fraction: f64,
}
#[derive(Debug, Serialize)]
pub struct PoleCoverage {
    pub radius_degrees: f64,
    pub zenith: CapCoverage,
    pub nadir: CapCoverage,
    pub detects_opaque_obstructions: bool,
}

/// Detect alpha coverage problems. Opaque tripods, people and duplicated texture cannot be
/// inferred from alpha and are explicitly outside this detector's evidence.
pub fn pole_coverage(source: &Panorama<'_>, radius_degrees: f64) -> Result<PoleCoverage> {
    if !radius_degrees.is_finite() || !(0.1..=60.0).contains(&radius_degrees) {
        return Err(Error::Invalid(
            "pole detection radius must be 0.1..60 degrees",
        ));
    }
    validate_pixels(source.data)?;
    let cap = |pole: Pole| {
        let mut report = CapCoverage::default();
        let (mut missing, mut area) = (0.0, 0.0);
        for y in 0..source.height {
            let angle = (f64::from(y) + 0.5) / f64::from(source.height) * PI;
            let distance = if pole == Pole::Zenith {
                angle
            } else {
                PI - angle
            };
            if distance > radius_degrees.to_radians() {
                continue;
            }
            let weight = angle.sin();
            for x in 0..source.width {
                let alpha = source.data[(y * source.width + x) as usize][3];
                report.sampled_pixels += 1;
                area += weight;
                if alpha <= 1e-6 {
                    report.missing_pixels += 1;
                    missing += weight;
                } else if alpha < 0.999 {
                    report.partial_pixels += 1;
                }
            }
        }
        report.missing_area_fraction = if area > 0.0 { missing / area } else { 0.0 };
        report
    };
    Ok(PoleCoverage {
        radius_degrees,
        zenith: cap(Pole::Zenith),
        nadir: cap(Pole::Nadir),
        detects_opaque_obstructions: false,
    })
}
