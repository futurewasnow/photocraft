//! Conservative dual-lens photometric matching in linear RGB, independent of view rotation.
use super::{Input, MeiLens, sample};
use crate::{Error, Result, pixel_count};
use serde::Serialize;

#[derive(Clone, Copy, Debug)]
pub struct Gains([[f32; 3]; 2]);
impl Gains {
    pub const IDENTITY: Self = Self([[1.; 3]; 2]);
    pub fn from_ratio_ev(ratio: [f64; 3]) -> Result<Self> {
        if ratio.iter().any(|v| !v.is_finite() || v.abs() > 1.) {
            return Err(Error::Invalid(
                "balance ratio must be finite and within one stop",
            ));
        }
        // Use a common attenuation after the symmetric correction: no channel is
        // amplified, so matching cannot introduce additional highlight clipping.
        // The common scalar preserves the geometric-mean chromaticity.
        let headroom = ratio.iter().map(|v| v.abs() * 0.5).fold(0f64, f64::max);
        Ok(Self([
            ratio.map(|v| (v * 0.5 - headroom).exp2() as f32),
            ratio.map(|v| (-v * 0.5 - headroom).exp2() as f32),
        ]))
    }
    pub fn rgb(self) -> [[f32; 3]; 2] {
        self.0
    }
}

struct Point {
    xy: [[f64; 2]; 2],
    sector: usize,
    validation: bool,
}
pub struct Plan {
    lenses: [MeiLens; 2],
    points: Vec<Point>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Estimate {
    pub accepted: bool,
    pub reason: &'static str,
    /// log2(lens1 / lens0), per channel. A common attenuation protects highlight headroom.
    pub ratio_ev: [f64; 3],
    pub candidate_points: usize,
    pub usable_points: usize,
    pub fit_inliers: usize,
    pub covered_sectors: usize,
    pub validation_points: usize,
    pub validation_before_ev: Option<[f64; 3]>,
    pub validation_after_ev: Option<[f64; 3]>,
}
impl Estimate {
    pub fn gains(&self) -> Result<Gains> {
        if self.accepted {
            Gains::from_ratio_ev(self.ratio_ev)
        } else {
            Ok(Gains::IDENTITY)
        }
    }
}
struct Observation {
    ratio: [f64; 3],
    sector: usize,
    validation: bool,
}
/// Fit RGB gains from locally matched overlap patches. Geometry remains unchanged.
pub fn estimate_aligned(alignment: &super::overlap::Alignment) -> Result<Estimate> {
    if alignment.width == 0
        || alignment.height == 0
        || alignment.width > 8192
        || alignment.height > 512
        || alignment.matches.len() > 131072
        || alignment.matches.len() > alignment.candidates
    {
        return Err(Error::Invalid("invalid alignment dimensions or count"));
    }
    let mut observations = Vec::new();
    observations
        .try_reserve_exact(alignment.matches.len())
        .map_err(|_| Error::Allocation)?;
    for m in &alignment.matches {
        if m.first[0] >= alignment.width
            || m.first[1] >= alignment.height
            || m.second.iter().any(|v| !v.is_finite())
            || !(0.0..alignment.width as f64).contains(&m.second[0])
            || !(0.0..alignment.height as f64).contains(&m.second[1])
            || !m.correlation.is_finite()
            || m.correlation < 0.9
            || m.correlation > 1.000001
            || !m.cycle_error_pixels.is_finite()
            || !(0.0..=1.).contains(&m.cycle_error_pixels)
        {
            return Err(Error::Invalid("invalid alignment correspondence"));
        }
        let (Some(a), Some(b)) = (m.first_rgb, m.second_rgb) else {
            continue;
        };
        // Atlas samples are f32 before averaging in f64. Preserve the lower
        // threshold's f32 rounding and allow only arithmetic-roundoff slack.
        let colour_range = (f64::from(0.005_f32) - 1e-12)..=(0.95 + 1e-12);
        if a.iter()
            .chain(&b)
            .any(|v| !v.is_finite() || !colour_range.contains(v))
        {
            return Err(Error::Invalid("invalid alignment colour sample"));
        }
        let ratio = std::array::from_fn(|c| (b[c] / a[c]).log2());
        if ratio.iter().all(|v| v.abs() <= 2.) {
            observations.push(Observation {
                ratio,
                sector: m.first[0] * 8 / alignment.width,
                validation: (m.first[0] / 8 + m.first[1] / 8) % 2 == 1,
            });
        }
    }
    fit(&observations, alignment.candidates)
}
fn median(mut values: Vec<f64>) -> f64 {
    values.sort_unstable_by(f64::total_cmp);
    let n = values.len();
    if n.is_multiple_of(2) {
        (values[n / 2 - 1] + values[n / 2]) * 0.5
    } else {
        values[n / 2]
    }
}
fn patch(input: &Input<'_>, xy: [f64; 2]) -> Option<[f64; 3]> {
    let mut sum = [0.; 3];
    for dy in [-2., 0., 2.] {
        for dx in [-2., 0., 2.] {
            let p = sample(input, xy[0] + dx, xy[1] + dy);
            if !p[3].is_finite()
                || !(0.99..=1.0001).contains(&p[3])
                || p[..3]
                    .iter()
                    .any(|v| !v.is_finite() || !(0.005..=0.95).contains(v))
            {
                return None;
            }
            for c in 0..3 {
                sum[c] += f64::from(p[c]);
            }
        }
    }
    Some(sum.map(|v| v / 9.))
}
impl Plan {
    pub fn new(lenses: [MeiLens; 2]) -> Result<Self> {
        for lens in &lenses {
            lens.validate()?;
        }
        let mut points = Vec::new();
        points
            .try_reserve_exact(8192)
            .map_err(|_| Error::Allocation)?;
        // Equal-area sphere samples avoid over-weighting the poles. Keep fitting in camera
        // coordinates so a stabilized output view cannot change the photometric observation.
        for row in 0..64 {
            let y = 1. - 2. * (row as f64 + 0.5) / 64.;
            let radius = (1. - y * y).sqrt();
            for column in 0..128 {
                let lon = (column as f64 + 0.5) * std::f64::consts::TAU / 128.;
                let d = [radius * lon.sin(), y, radius * lon.cos()];
                let projected = lenses.each_ref().map(|lens| lens.project(d));
                let [Some(a), Some(b)] = projected else {
                    continue;
                };
                let locations = [a, b];
                if locations.iter().zip(&lenses).any(|(&(x, y, theta), lens)| {
                    theta > 95f64.to_radians()
                        || x < 3.
                        || y < 3.
                        || x > f64::from(lens.width) - 4.
                        || y > f64::from(lens.height) - 4.
                }) {
                    continue;
                }
                let local = crate::rotate(lenses[0].world_to_lens, d);
                let angle = local[1].atan2(local[0]).rem_euclid(std::f64::consts::TAU);
                let sector = ((angle / std::f64::consts::TAU * 8.) as usize).min(7);
                points.push(Point {
                    xy: [[a.0, a.1], [b.0, b.1]],
                    sector,
                    validation: (row + column) % 2 == 1,
                });
            }
        }
        Ok(Self { lenses, points })
    }
    pub fn estimate(&self, sources: [&[[f32; 4]]; 2]) -> Result<Estimate> {
        for (lens, source) in self.lenses.iter().zip(sources) {
            if source.len() != pixel_count(lens.width, lens.height)? {
                return Err(Error::Invalid("balance source dimensions mismatch"));
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
        let mut observations = Vec::new();
        observations
            .try_reserve_exact(self.points.len())
            .map_err(|_| Error::Allocation)?;
        for p in &self.points {
            let (Some(a), Some(b)) = (patch(&inputs[0], p.xy[0]), patch(&inputs[1], p.xy[1]))
            else {
                continue;
            };
            let ratio = std::array::from_fn(|c| (b[c] / a[c]).log2());
            if ratio.iter().all(|v| v.abs() <= 2.) {
                observations.push(Observation {
                    ratio,
                    sector: p.sector,
                    validation: p.validation,
                });
            }
        }
        fit(&observations, self.points.len())
    }
}
fn fit(observations: &[Observation], candidates: usize) -> Result<Estimate> {
    let mut result = Estimate {
        accepted: false,
        reason: "insufficient_reliable_overlap",
        ratio_ev: [0.; 3],
        candidate_points: candidates,
        usable_points: observations.len(),
        fit_inliers: 0,
        covered_sectors: 0,
        validation_points: 0,
        validation_before_ev: None,
        validation_after_ev: None,
    };
    let train: Vec<_> = observations.iter().filter(|o| !o.validation).collect();
    let validation: Vec<_> = observations.iter().filter(|o| o.validation).collect();
    result.validation_points = validation.len();
    if train.len() < 64 || validation.len() < 32 {
        return Ok(result);
    }
    let initial: [f64; 3] =
        std::array::from_fn(|c| median(train.iter().map(|o| o.ratio[c]).collect()));
    let inliers: Vec<_> = train
        .iter()
        .filter(|o| (0..3).all(|c| (o.ratio[c] - initial[c]).abs() <= 0.25))
        .collect();
    result.fit_inliers = inliers.len();
    if inliers.len() < 64 || inliers.len() * 100 < train.len() * 65 {
        result.reason = "inconsistent_overlap";
        return Ok(result);
    }
    let mut sectors = [0usize; 8];
    for o in &inliers {
        sectors[o.sector] += 1;
    }
    result.covered_sectors = sectors.iter().filter(|n| **n >= 8).count();
    if result.covered_sectors < 4 {
        result.reason = "insufficient_spatial_coverage";
        return Ok(result);
    }
    let ratio: [f64; 3] =
        std::array::from_fn(|c| median(inliers.iter().map(|o| o.ratio[c]).collect()));
    if ratio.iter().any(|v| v.abs() > 1.) {
        result.reason = "correction_exceeds_one_stop";
        return Ok(result);
    }
    let mad: [f64; 3] = std::array::from_fn(|c| {
        median(
            inliers
                .iter()
                .map(|o| (o.ratio[c] - ratio[c]).abs())
                .collect(),
        )
    });
    if mad.iter().any(|v| *v > 0.12) {
        result.reason = "unstable_channel_fit";
        return Ok(result);
    }
    let before: [f64; 3] =
        std::array::from_fn(|c| median(validation.iter().map(|o| o.ratio[c].abs()).collect()));
    let after: [f64; 3] = std::array::from_fn(|c| {
        median(
            validation
                .iter()
                .map(|o| (o.ratio[c] - ratio[c]).abs())
                .collect(),
        )
    });
    result.validation_before_ev = Some(before);
    result.validation_after_ev = Some(after);
    if ratio.iter().all(|v| v.abs() < 0.02) {
        result.accepted = true;
        result.reason = "already_matched";
        // The deadband applies identity, so report its actual residual rather
        // than the hypothetical improvement of a correction we did not use.
        result.validation_after_ev = Some(before);
        return Ok(result);
    }
    if (0..3).any(|c| after[c] > before[c] + 0.01)
        || after.iter().sum::<f64>() > before.iter().sum::<f64>() * 0.95
    {
        result.reason = "held_out_fit_did_not_improve";
        return Ok(result);
    }
    result.ratio_ev = ratio;
    result.accepted = true;
    result.reason = "matched";
    Ok(result)
}

/// Bounded video adaptation in log gain. Rejected fits hold the previous correction.
#[derive(Default)]
pub struct Temporal {
    ratio: [f64; 3],
    started: bool,
}
impl Temporal {
    pub fn update(&mut self, estimate: &Estimate, seconds: f64) -> Result<Gains> {
        if !seconds.is_finite() || !(0.0..=60.).contains(&seconds) || seconds == 0. {
            return Err(Error::Invalid("invalid balance frame duration"));
        }
        if estimate.accepted {
            Gains::from_ratio_ev(estimate.ratio_ev)?;
            if !self.started {
                self.ratio = estimate.ratio_ev;
            } else {
                let alpha = 1. - (-seconds / 2.).exp();
                for c in 0..3 {
                    self.ratio[c] += ((estimate.ratio_ev[c] - self.ratio[c]) * alpha)
                        .clamp(-0.1 * seconds, 0.1 * seconds);
                }
            }
        }
        self.started = true;
        Gains::from_ratio_ev(self.ratio)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matched_correspondences_fit_and_malformed_reports_fail() -> Result<()> {
        let mut alignment = super::super::overlap::Alignment {
            width: 2048,
            height: 128,
            vertical_span_degrees: 24.,
            candidates: 256,
            matches: (0..256)
                .map(|i| super::super::overlap::Match {
                    first: [i / 2 * 16, 32 + i % 2 * 8],
                    second: [(i / 2 * 16 + 3) as f64, (30 + i % 2 * 8) as f64],
                    correlation: 0.98,
                    cycle_error_pixels: 0.2,
                    first_rgb: Some([0.1, 0.15, 0.2]),
                    second_rgb: Some([0.14, 0.12, 0.22]),
                })
                .collect(),
        };
        let fit = estimate_aligned(&alignment)?;
        assert!(fit.accepted, "{fit:?}");
        for (actual, expected) in fit.ratio_ev.iter().zip([1.4f64, 0.8, 1.1]) {
            assert!((actual - expected.log2()).abs() < 1e-12);
        }
        alignment.matches[0].first_rgb = Some([f64::from(0.005_f32); 3]);
        alignment.matches[0].second_rgb = Some([f64::from(0.005_f32); 3]);
        assert!(estimate_aligned(&alignment)?.accepted);
        alignment.matches[0].second_rgb = None;
        assert!(estimate_aligned(&alignment)?.accepted);
        alignment.matches[0].second[0] = 2048.;
        assert!(estimate_aligned(&alignment).is_err());
        alignment.matches[0].second[0] = 3.;
        alignment.matches[0].second_rgb = Some([f64::NAN, 0.12, 0.22]);
        assert!(estimate_aligned(&alignment).is_err());
        alignment.width = 0;
        assert!(estimate_aligned(&alignment).is_err());
        Ok(())
    }
    fn lenses() -> [MeiLens; 2] {
        std::array::from_fn(|i| MeiLens {
            width: 256,
            height: 256,
            world_to_lens: if i == 0 {
                [[1., 0., 0.], [0., -1., 0.], [0., 0., 1.]]
            } else {
                [[-1., 0., 0.], [0., -1., 0.], [0., 0., -1.]]
            },
            xi: 1.,
            focal: [100.; 2],
            center: [128.; 2],
            radial: [0.; 3],
            tangential: [0.; 2],
            max_theta_degrees: 100.,
        })
    }
    #[test]
    fn directional_scene_uses_corresponding_rays_in_both_lenses() -> Result<()> {
        let lens = lenses();
        let ratios = [1.4f64, 0.8, 1.1];
        let buffers: [Vec<[f32; 4]>; 2] = std::array::from_fn(|j| {
            (0..256 * 256)
                .map(|i| {
                    let x = ((i % 256) as f64 + 0.5 - 128.) / 100.;
                    let y = ((i / 256) as f64 + 0.5 - 128.) / 100.;
                    let r2 = x * x + y * y;
                    let local = [
                        2. * x / (1. + r2),
                        2. * y / (1. + r2),
                        (1. - r2) / (1. + r2),
                    ];
                    // These synthetic transforms are diagonal and their own inverse.
                    let world = crate::rotate(lens[j].world_to_lens, local);
                    let mut pixel = [1.; 4];
                    for c in 0..3 {
                        pixel[c] =
                            ((0.2 + 0.05 * world[c]) * if j == 1 { ratios[c] } else { 1. }) as f32;
                    }
                    pixel
                })
                .collect()
        });
        let e = Plan::new(lens)?.estimate([&buffers[0], &buffers[1]])?;
        assert!(e.accepted, "{e:?}");
        for (actual, expected) in e.ratio_ev.iter().zip(ratios) {
            assert!((actual - expected.log2()).abs() < 0.001, "{e:?}");
        }
        Ok(())
    }
    #[test]
    fn known_rgb_mismatch_is_recovered_and_alpha_and_highlights_are_preserved() -> Result<()> {
        let lens = lenses();
        let a = [0.1, 0.15, 0.2, 1.];
        let ratio = [1.4f32, 0.8, 1.1];
        let b = [a[0] * ratio[0], a[1] * ratio[1], a[2] * ratio[2], 1.];
        let buffers = [vec![a; 256 * 256], vec![b; 256 * 256]];
        let plan = Plan::new(lens.clone())?;
        let estimate = plan.estimate([&buffers[0], &buffers[1]])?;
        assert!(estimate.accepted, "{estimate:?}");
        assert_eq!(estimate.reason, "matched");
        let gains = estimate.gains()?;
        for c in 0..3 {
            assert!((estimate.ratio_ev[c] - f64::from(ratio[c]).log2()).abs() < 1e-6);
            assert!((a[c] * gains.rgb()[0][c] - b[c] * gains.rgb()[1][c]).abs() < 1e-6);
        }
        assert!(gains.rgb().iter().flatten().all(|v| (0.5..=1.).contains(v)));
        let map = super::super::DualMap::new(lens, 128, 64)?;
        let mut pixels = crate::pixels(128, 64)?;
        assert_eq!(
            map.render_balanced([&buffers[0], &buffers[1]], &mut pixels, gains)?,
            0
        );
        for p in &pixels {
            assert!((p[3] - 1.).abs() < 1e-6);
            for c in 0..3 {
                assert!((p[c] - a[c] * gains.rgb()[0][c]).abs() < 1e-6);
            }
        }
        for value in [0., 1., f32::NAN] {
            let bad = vec![[value, value, value, 1.]; 256 * 256];
            let e = plan.estimate([&bad, &bad])?;
            assert!(!e.accepted);
            assert_eq!(e.gains()?.rgb(), Gains::IDENTITY.rgb());
        }
        assert!(plan.estimate([&buffers[0][..10], &buffers[1]]).is_err());
        assert!(Gains::from_ratio_ev([f64::NAN, 0., 0.]).is_err());
        assert!(Gains::from_ratio_ev([1.01, 0., 0.]).is_err());
        Ok(())
    }
    fn observations() -> Vec<Observation> {
        (0..800)
            .map(|i| Observation {
                ratio: if i % 10 == 0 { [1.5; 3] } else { [0.4; 3] },
                sector: (i / 2) % 8,
                validation: i % 2 == 1,
            })
            .collect()
    }
    #[test]
    fn outliers_sparse_coverage_and_failed_held_out_fit_are_handled() -> Result<()> {
        let mut points = observations();
        let e = fit(&points, points.len())?;
        assert!(e.accepted, "{e:?}");
        assert_eq!(e.ratio_ev, [0.4; 3]);
        let matched: Vec<_> = (0..800)
            .map(|i| Observation {
                ratio: [0.01; 3],
                sector: (i / 2) % 8,
                validation: i % 2 == 1,
            })
            .collect();
        let near = fit(&matched, matched.len())?;
        assert!(near.accepted);
        assert_eq!(near.reason, "already_matched");
        assert_eq!(near.gains()?.rgb(), Gains::IDENTITY.rgb());
        assert_eq!(near.validation_after_ev, near.validation_before_ev);
        for p in &mut points {
            if p.validation {
                p.ratio = [-0.4; 3];
            }
        }
        let e = fit(&points, points.len())?;
        assert!(!e.accepted);
        assert_eq!(e.reason, "held_out_fit_did_not_improve");
        for p in &mut points {
            p.ratio = [0.4; 3];
            p.sector = 0;
        }
        assert_eq!(
            fit(&points, points.len())?.reason,
            "insufficient_spatial_coverage"
        );
        assert!(!fit(&points[..20], 20)?.accepted);
        Ok(())
    }
    #[test]
    fn temporal_changes_are_bounded_and_rejected_fits_hold_previous_state() -> Result<()> {
        let mut e = fit(&observations(), 800)?;
        let mut filter = Temporal::default();
        let first = filter.update(&e, 1. / 24.)?;
        assert_eq!(first.rgb(), e.gains()?.rgb());
        e.ratio_ev = [-0.4; 3];
        filter.update(&e, 1. / 24.)?;
        assert!((filter.ratio[0] - (0.4 - 0.1 / 24.)).abs() < 1e-12);
        let previous = filter.ratio;
        e.accepted = false;
        filter.update(&e, 1. / 24.)?;
        assert_eq!(filter.ratio, previous);
        assert!(filter.update(&e, 0.).is_err());
        let mut late = Temporal::default();
        late.update(&e, 1. / 24.)?;
        e.accepted = true;
        late.update(&e, 1. / 24.)?;
        assert!((late.ratio[0] + 0.1 / 24.).abs() < 1e-12);
        Ok(())
    }
}
