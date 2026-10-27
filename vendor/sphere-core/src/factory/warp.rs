//! Experimental sparse seam warping. Held-out matches and a bounded field gradient
//! gate deformation; originals are sampled directly, without an intermediate panorama.
use super::{Input, MeiLens, overlap::Alignment, sample};
use crate::parallel::*;
use crate::{Error, Result, direction, pixel_count, pixels, rotate, stitch::Stitched};
use serde::Serialize;
use std::collections::BTreeSet;

const WIDTH: usize = 2048;
const HEIGHT: usize = 128;
const STEP: usize = 8;
const GX: usize = WIDTH / STEP;
const GY: usize = HEIGHT / STEP + 1;
const SPAN: f64 = 24.;

#[derive(Clone, Copy)]
struct Point {
    midpoint: [f64; 2],
    flow: [f64; 2],
    validation: bool,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub accepted: bool,
    pub reason: &'static str,
    pub training_matches: usize,
    pub validation_matches: usize,
    pub supported_validation: usize,
    pub supported_validation_sectors: usize,
    pub supported_nodes: usize,
    pub median_before_pixels: Option<f64>,
    pub median_after_pixels: Option<f64>,
    pub p90_before_pixels: Option<f64>,
    pub p90_after_pixels: Option<f64>,
    pub worsened_by_half_pixel: usize,
    pub field_scale: f64,
    pub maximum_field_gradient: f64,
    /// Determinants are in the angular analysis strip, not source-sensor pixels.
    pub minimum_strip_jacobian: f64,
    pub maximum_strip_jacobian: f64,
}
#[derive(Serialize)]
pub struct Warp {
    #[serde(skip)]
    lenses: [MeiLens; 2],
    report: Report,
    grid_width: usize,
    grid_height: usize,
    grid_step: usize,
    field: Vec<[f64; 2]>,
    #[serde(skip)]
    validation: Vec<Point>,
}
pub struct Rendered {
    pub stitched: Stitched,
    pub warped_pixels: usize,
}
/// Causal camera-space field. A common blend factor preserves the spatial
/// gradient bound; independent per-node clipping would not preserve that bound.
pub struct Temporal {
    pub(super) lenses: [MeiLens; 2],
    field: Vec<[f64; 2]>,
    initialized: bool,
}
#[derive(Debug, Serialize)]
pub struct TemporalReport {
    pub candidate: Report,
    pub action: &'static str,
    /// Zero for initialization, which has no preceding delivered frame.
    pub maximum_step_pixels: f64,
    pub maximum_field_gradient: f64,
    pub minimum_strip_jacobian: f64,
    pub maximum_strip_jacobian: f64,
    pub held_out_points: usize,
    pub median_before_pixels: Option<f64>,
    pub median_after_pixels: Option<f64>,
    pub p90_after_pixels: Option<f64>,
    pub worsened_by_half_pixel: usize,
    /// Current-frame observations support improvement of the delivered temporal field.
    /// False also includes insufficient observations; it does not establish a regression.
    pub current_alignment_improved: bool,
}
impl Temporal {
    pub fn new(lenses: [MeiLens; 2]) -> Result<Self> {
        for lens in &lenses {
            lens.validate()?;
        }
        Ok(Self {
            lenses,
            field: vec![[0.; 2]; GX * GY],
            initialized: false,
        })
    }
    pub fn update(&mut self, candidate: Warp, seconds: f64) -> Result<TemporalReport> {
        if !seconds.is_finite() || !(0.0..=1.).contains(&seconds) || seconds == 0. {
            return Err(Error::Invalid(
                "seam update interval must be positive and at most one second",
            ));
        }
        if self.lenses != candidate.lenses {
            return Err(Error::Invalid("seam temporal calibration changed"));
        }
        let accepted = candidate.report.accepted;
        let target: Vec<_> = candidate
            .field
            .iter()
            .map(|v| if accepted { *v } else { [0.; 2] })
            .collect();
        let initializing = !self.initialized;
        let maximum_delta = self
            .field
            .iter()
            .zip(&target)
            .map(|(a, b)| norm([b[0] - a[0], b[1] - a[1]]))
            .fold(0f64, f64::max);
        let blend = if initializing {
            1.
        } else {
            (1. - (-seconds / 0.25).exp()).min(if maximum_delta > 0. {
                2. * seconds / maximum_delta
            } else {
                1.
            })
        };
        for (value, target) in self.field.iter_mut().zip(target) {
            for c in 0..2 {
                value[c] += (target[c] - value[c]) * blend;
            }
        }
        self.initialized = true;
        let (gradient, minimum, maximum) = derivatives(&self.field);
        let mut before = Vec::new();
        let mut after = Vec::new();
        let mut worsened = 0;
        for p in &candidate.validation {
            let flow = interpolate(&self.field, p.midpoint);
            let a = norm(p.flow);
            let b = norm([p.flow[0] - flow[0], p.flow[1] - flow[1]]);
            worsened += usize::from(b > a + 0.5);
            before.push(a);
            after.push(b);
        }
        let count = before.len();
        let mb = (!before.is_empty()).then(|| percentile(before.clone(), 0.5));
        let ma = (!after.is_empty()).then(|| percentile(after.clone(), 0.5));
        let pb = (!before.is_empty()).then(|| percentile(before, 0.9));
        let pa = (!after.is_empty()).then(|| percentile(after, 0.9));
        let improved = count >= 12
            && matches!((mb, ma, pb, pa), (Some(b), Some(a), Some(qb), Some(qa))
            if b > 0.1 && a <= b * 0.95 && qa <= qb + 0.1 && worsened * 10 <= count);
        Ok(TemporalReport {
            candidate: candidate.report,
            action: if initializing {
                "initialized"
            } else if accepted {
                "tracking"
            } else {
                "fading_to_factory"
            },
            maximum_step_pixels: if initializing {
                0.
            } else {
                maximum_delta * blend
            },
            maximum_field_gradient: gradient,
            minimum_strip_jacobian: minimum,
            maximum_strip_jacobian: maximum,
            held_out_points: count,
            median_before_pixels: mb,
            median_after_pixels: ma,
            p90_after_pixels: pa,
            worsened_by_half_pixel: worsened,
            current_alignment_improved: improved,
        })
    }
    pub(super) fn rays(&self, d: [f64; 3]) -> ([f64; 3], [f64; 3], bool) {
        rays(&self.lenses, &self.field, d)
    }
}
fn wrapped(x: f64) -> f64 {
    (x + WIDTH as f64 / 2.).rem_euclid(WIDTH as f64) - WIDTH as f64 / 2.
}
fn norm(x: [f64; 2]) -> f64 {
    x[0].hypot(x[1])
}
fn percentile(mut values: Vec<f64>, fraction: f64) -> f64 {
    values.sort_unstable_by(f64::total_cmp);
    let index = fraction * (values.len() - 1) as f64;
    let lower = index.floor() as usize;
    let upper = index.ceil() as usize;
    values[lower] + (values[upper] - values[lower]) * index.fract()
}
fn interpolate(field: &[[f64; 2]], p: [f64; 2]) -> [f64; 2] {
    if p[1] < 0. || p[1] >= HEIGHT as f64 {
        return [0.; 2];
    }
    let x = p[0].rem_euclid(WIDTH as f64) / STEP as f64;
    let y = p[1] / STEP as f64;
    let ix = x.floor() as usize;
    let iy = y.floor() as usize;
    let fx = x.fract();
    let fy = y.fract();
    let next = (ix + 1) % GX;
    std::array::from_fn(|c| {
        (field[iy * GX + ix][c] * (1. - fx) + field[iy * GX + next][c] * fx) * (1. - fy)
            + (field[(iy + 1) * GX + ix][c] * (1. - fx) + field[(iy + 1) * GX + next][c] * fx) * fy
    })
}
/// Maximum gradient norm and both symmetric maps' determinant extrema, at all
/// bilinear-cell corners. Convexity bounds the norm throughout every cell.
fn derivatives(field: &[[f64; 2]]) -> (f64, f64, f64) {
    let (mut max_gradient, mut minimum, mut maximum) = (0f64, 1f64, 1f64);
    for y in 0..GY - 1 {
        for x in 0..GX {
            let next = (x + 1) % GX;
            let dx = [
                std::array::from_fn::<_, 2, _>(|c| {
                    (field[y * GX + next][c] - field[y * GX + x][c]) / STEP as f64
                }),
                std::array::from_fn(|c| {
                    (field[(y + 1) * GX + next][c] - field[(y + 1) * GX + x][c]) / STEP as f64
                }),
            ];
            let dy = [
                std::array::from_fn::<_, 2, _>(|c| {
                    (field[(y + 1) * GX + x][c] - field[y * GX + x][c]) / STEP as f64
                }),
                std::array::from_fn(|c| {
                    (field[(y + 1) * GX + next][c] - field[y * GX + next][c]) / STEP as f64
                }),
            ];
            for u in dx {
                for v in dy {
                    max_gradient = max_gradient
                        .max((u[0] * u[0] + u[1] * u[1] + v[0] * v[0] + v[1] * v[1]).sqrt());
                    for sign in [-0.5, 0.5] {
                        let determinant =
                            (1. + sign * u[0]) * (1. + sign * v[1]) - sign * sign * u[1] * v[0];
                        minimum = minimum.min(determinant);
                        maximum = maximum.max(determinant);
                    }
                }
            }
        }
    }
    (max_gradient, minimum, maximum)
}
impl Warp {
    pub fn fit(alignment: &Alignment, lenses: [MeiLens; 2]) -> Result<Self> {
        for lens in &lenses {
            lens.validate()?;
        }
        if alignment.width != WIDTH
            || alignment.height != HEIGHT
            || alignment.vertical_span_degrees != SPAN
            || alignment.matches.len() > 3072
            || alignment.matches.len() > alignment.candidates
        {
            return Err(Error::Invalid("unsupported seam alignment geometry"));
        }
        let mut points = Vec::new();
        points
            .try_reserve_exact(alignment.matches.len())
            .map_err(|_| Error::Allocation)?;
        let mut seen = BTreeSet::new();
        for m in &alignment.matches {
            if m.first[0] >= WIDTH
                || m.first[1] >= HEIGHT
                || m.second.iter().any(|v| !v.is_finite())
                || !(0.0..WIDTH as f64).contains(&m.second[0])
                || !(0.0..HEIGHT as f64).contains(&m.second[1])
                || !m.correlation.is_finite()
                || !(0.9..=1.000001).contains(&m.correlation)
                || !m.cycle_error_pixels.is_finite()
                || !(0.0..=1.).contains(&m.cycle_error_pixels)
                || !seen.insert(m.first)
            {
                return Err(Error::Invalid("invalid or duplicate seam correspondence"));
            }
            let flow = [
                wrapped(m.second[0] - m.first[0] as f64),
                m.second[1] - m.first[1] as f64,
            ];
            if flow.iter().any(|v| v.abs() > 7.500001) {
                return Err(Error::Invalid("seam correspondence exceeds local search"));
            }
            points.push(Point {
                midpoint: [
                    (m.first[0] as f64 + flow[0] * 0.5).rem_euclid(WIDTH as f64),
                    m.first[1] as f64 + flow[1] * 0.5,
                ],
                flow,
                validation: (m.first[0] / 8 + m.first[1] / 8) % 4 == 0,
            });
        }
        let training: Vec<_> = points.iter().filter(|p| !p.validation).collect();
        let validation: Vec<_> = points.iter().filter(|p| p.validation).collect();
        let mut report = Report {
            accepted: false,
            reason: "insufficient_validation_support",
            training_matches: training.len(),
            validation_matches: validation.len(),
            supported_validation: 0,
            supported_validation_sectors: 0,
            supported_nodes: 0,
            median_before_pixels: None,
            median_after_pixels: None,
            p90_before_pixels: None,
            p90_after_pixels: None,
            worsened_by_half_pixel: 0,
            field_scale: 1.,
            maximum_field_gradient: 0.,
            minimum_strip_jacobian: 1.,
            maximum_strip_jacobian: 1.,
        };
        let mut field = Vec::new();
        field
            .try_reserve_exact(GX * GY)
            .map_err(|_| Error::Allocation)?;
        field.resize(GX * GY, [0.; 2]);
        if training.len() >= 24 && validation.len() >= 12 {
            field.par_iter_mut().enumerate().for_each(|(i, value)| {
                let (x, y) = ((i % GX) * STEP, (i / GX) * STEP);
                let beta = ((0.5 - (y as f64 + 0.5) / HEIGHT as f64) * SPAN).abs();
                let t = ((7. - beta) / 4.).clamp(0., 1.);
                let taper = t * t * (3. - 2. * t);
                if taper == 0. || y == 0 || y == HEIGHT {
                    return;
                }
                let nearby: Vec<_> = training
                    .iter()
                    .filter_map(|p| {
                        let distance = (wrapped(p.midpoint[0] - x as f64) / 80.).powi(2)
                            + ((p.midpoint[1] - y as f64) / 32.).powi(2);
                        (distance < 1.).then_some((p, distance))
                    })
                    .collect();
                if nearby.len() < 3 {
                    return;
                }
                let median: [f64; 2] = std::array::from_fn(|c| {
                    percentile(nearby.iter().map(|(p, _)| p.flow[c]).collect(), 0.5)
                });
                let inliers: Vec<_> = nearby
                    .iter()
                    .filter(|(p, _)| norm([p.flow[0] - median[0], p.flow[1] - median[1]]) <= 1.5)
                    .collect();
                if inliers.len() < 3 || inliers.len() * 100 < nearby.len() * 65 {
                    return;
                }
                let weight = inliers.iter().map(|(_, d)| (1. - d).powi(2)).sum::<f64>();
                if weight < 0.5 {
                    return;
                }
                let confidence = (weight / 2.).min(1.);
                *value = std::array::from_fn(|c| {
                    inliers
                        .iter()
                        .map(|(p, d)| p.flow[c] * (1. - d).powi(2))
                        .sum::<f64>()
                        / weight
                        * confidence
                        * taper
                });
            });
            report.supported_nodes = field.iter().filter(|p| norm(**p) > 1e-9).count();
            let gradient = derivatives(&field).0;
            report.field_scale = if gradient > 0.5 { 0.5 / gradient } else { 1. };
            for value in &mut field {
                for v in value {
                    *v *= report.field_scale;
                }
            }
            let (gradient, minimum, maximum) = derivatives(&field);
            report.maximum_field_gradient = gradient;
            report.minimum_strip_jacobian = minimum;
            report.maximum_strip_jacobian = maximum;
            let mut before = Vec::new();
            let mut after = Vec::new();
            let mut sectors = BTreeSet::new();
            for p in &validation {
                let predicted = interpolate(&field, p.midpoint);
                let a = norm(p.flow);
                let b = norm([p.flow[0] - predicted[0], p.flow[1] - predicted[1]]);
                before.push(a);
                after.push(b);
                if norm(predicted) > 0.1 {
                    report.supported_validation += 1;
                    sectors.insert((p.midpoint[0] * 8. / WIDTH as f64) as usize);
                }
                report.worsened_by_half_pixel += usize::from(b > a + 0.5);
            }
            report.supported_validation_sectors = sectors.len();
            let mb = percentile(before.clone(), 0.5);
            let ma = percentile(after.clone(), 0.5);
            let pb = percentile(before, 0.9);
            let pa = percentile(after, 0.9);
            report.median_before_pixels = Some(mb);
            report.median_after_pixels = Some(ma);
            report.p90_before_pixels = Some(pb);
            report.p90_after_pixels = Some(pa);
            if report.supported_validation >= 12 && sectors.len() >= 3 {
                report.reason = "held_out_alignment_did_not_improve";
                if mb > 0.1
                    && ma <= mb * 0.75
                    && pa <= pb + 0.1
                    && report.worsened_by_half_pixel * 10 <= validation.len()
                    && gradient <= 0.50000001
                    && minimum >= 0.56249999
                {
                    report.accepted = true;
                    report.reason = "validated_local_warp";
                }
            }
        }
        Ok(Self {
            lenses,
            report,
            grid_width: GX,
            grid_height: GY,
            grid_step: STEP,
            field,
            validation: points.into_iter().filter(|p| p.validation).collect(),
        })
    }
    pub fn report(&self) -> &Report {
        &self.report
    }
    fn rays(&self, d: [f64; 3]) -> ([f64; 3], [f64; 3], bool) {
        rays(&self.lenses, &self.field, d)
    }
}
fn ray(lenses: &[MeiLens; 2], p: [f64; 2]) -> [f64; 3] {
    let phi = (p[0] + 0.5) / WIDTH as f64 * std::f64::consts::TAU;
    let beta = (0.5 - (p[1] + 0.5) / HEIGHT as f64) * SPAN.to_radians();
    let local = [beta.cos() * phi.cos(), beta.cos() * phi.sin(), beta.sin()];
    std::array::from_fn(|i| {
        (0..3)
            .map(|j| lenses[0].world_to_lens[j][i] * local[j])
            .sum()
    })
}
fn rays(lenses: &[MeiLens; 2], field: &[[f64; 2]], d: [f64; 3]) -> ([f64; 3], [f64; 3], bool) {
    let local = rotate(lenses[0].world_to_lens, d);
    if local[2].abs() >= 9f64.to_radians().sin() {
        return (d, d, false);
    }
    let phi = local[1].atan2(local[0]).rem_euclid(std::f64::consts::TAU);
    let beta = local[2].clamp(-1., 1.).asin();
    let p = [
        (phi / std::f64::consts::TAU * WIDTH as f64 - 0.5).rem_euclid(WIDTH as f64),
        (0.5 - beta / SPAN.to_radians()) * HEIGHT as f64 - 0.5,
    ];
    let flow = interpolate(field, p);
    if norm(flow) <= 1e-9 {
        return (d, d, false);
    }
    (
        ray(lenses, [p[0] - flow[0] * 0.5, p[1] - flow[1] * 0.5]),
        ray(lenses, [p[0] + flow[0] * 0.5, p[1] + flow[1] * 0.5]),
        true,
    )
}
impl Warp {
    pub fn render(&self, sources: [&[[f32; 4]]; 2], width: u32, height: u32) -> Result<Rendered> {
        if u64::from(width) != 2 * u64::from(height) {
            return Err(Error::Invalid("seam output must be 2:1"));
        }
        for (lens, source) in self.lenses.iter().zip(sources) {
            if source.len() != pixel_count(lens.width, lens.height)? {
                return Err(Error::Invalid("seam source dimensions mismatch"));
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
        if !self.report.accepted {
            return Ok(Rendered {
                stitched: super::stitch(&inputs, width, height)?,
                warped_pixels: 0,
            });
        }
        let mut output = pixels(width, height)?;
        let counts = output.par_iter_mut().enumerate().map(|(i, out)| {
            let d = direction(
                ((i % width as usize) as f64 + 0.5) / f64::from(width),
                ((i / width as usize) as f64 + 0.5) / f64::from(height),
            );
            let (a, b, warped) = self.rays(d);
            let rays = [a, b];
            let mut sum = [0.; 4];
            let mut weight = 0f32;
            let mut fallback = None;
            let mut invalid = 0usize;
            for (j, input) in inputs.iter().enumerate() {
                if let Some((x, y, theta)) = input.lens.project(d) {
                    let point = if warped {
                        input.lens.project(rays[j])
                    } else {
                        Some((x, y, theta))
                    };
                    let Some((u, v, _)) = point else {
                        invalid += 1;
                        continue;
                    };
                    let pixel = sample(input, u, v);
                    if fallback.is_none_or(|(angle, _)| theta < angle) {
                        fallback = Some((theta, pixel));
                    }
                    let t = ((theta.to_degrees() - 85.) / 10.).clamp(0., 1.);
                    let w = (1. - t * t * (3. - 2. * t)) as f32;
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
            (usize::from(out[3] <= 1e-12), usize::from(warped), invalid)
        });
        let counts = crate::parallel::reduce(
            counts,
            || (0, 0, 0),
            |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2),
        );
        if counts.2 > 0 {
            return Err(Error::Invalid("seam warp left calibrated lens coverage"));
        }
        Ok(Rendered {
            stitched: Stitched {
                pixels: output,
                uncovered_pixels: counts.0,
            },
            warped_pixels: counts.1,
        })
    }
}

#[cfg(test)]
mod tests;
