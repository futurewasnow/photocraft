use super::*;
use crate::factory::overlap::Match;

#[test]
fn temporal_slew_fade_and_geometry_guards_preserve_continuity() -> Result<()> {
    let lens = lenses(64);
    let mut filter = Temporal::new(lens.clone())?;
    let report = filter.update(Warp::fit(&alignment(false), lens.clone())?, 1. / 24.)?;
    assert!(report.current_alignment_improved);
    assert_eq!(report.action, "initialized");
    let mut reverse = alignment(false);
    for m in &mut reverse.matches {
        m.second = [
            (m.first[0] as f64 - 2.).rem_euclid(WIDTH as f64),
            m.first[1] as f64 + 3.,
        ];
    }
    let before = filter.field.clone();
    let report = filter.update(Warp::fit(&reverse, lens.clone())?, 1. / 24.)?;
    assert!(report.candidate.accepted);
    assert!(
        !report.current_alignment_improved,
        "stale opposite field must not be called improved"
    );
    assert!(report.maximum_step_pixels <= 2. / 24. + 1e-12);
    for x in (0..WIDTH).step_by(13) {
        for y in (0..HEIGHT).step_by(7) {
            let p = [x as f64 + 0.3, y as f64 + 0.2];
            let a = interpolate(&before, p);
            let b = interpolate(&filter.field, p);
            assert!(norm([a[0] - b[0], a[1] - b[1]]) <= 2. / 24. + 1e-12);
        }
    }
    let mut rejected = alignment(false);
    rejected.matches.clear();
    let mut previous_magnitude = filter.field.iter().map(|v| norm(*v)).fold(0f64, f64::max);
    for _ in 0..240 {
        let report = filter.update(Warp::fit(&rejected, lens.clone())?, 1. / 24.)?;
        assert_eq!(report.action, "fading_to_factory");
        assert!(!report.current_alignment_improved);
        assert!(report.maximum_field_gradient <= 0.50000001);
        assert!(report.minimum_strip_jacobian >= 0.56249999);
        assert!(report.maximum_step_pixels <= 2. / 24. + 1e-12);
        let magnitude = filter.field.iter().map(|v| norm(*v)).fold(0f64, f64::max);
        assert!(magnitude <= previous_magnitude);
        previous_magnitude = magnitude;
    }
    assert!(previous_magnitude < 1e-8);
    let unchanged = filter.field.clone();
    for dt in [0., -1., f64::NAN, f64::INFINITY, 1.01] {
        assert!(
            filter
                .update(Warp::fit(&alignment(false), lens.clone())?, dt)
                .is_err()
        );
        assert_eq!(filter.field, unchanged);
    }
    let mut different = lens.clone();
    different[0].focal[0] += 1.;
    assert!(
        filter
            .update(Warp::fit(&alignment(false), different.clone())?, 1. / 24.)
            .is_err()
    );
    assert_eq!(filter.field, unchanged);
    let mut wrong_map = super::super::DualMap::new(different, 128, 64)?;
    assert!(
        wrong_map
            .set_view_warp(crate::rotation::Rotation::IDENTITY, &filter)
            .is_err()
    );
    Ok(())
}

#[test]
fn temporal_map_composes_yaw_with_direct_original_lens_sampling() -> Result<()> {
    let lens = lenses(128);
    let buffers: [Vec<[f32; 4]>; 2] = std::array::from_fn(|j| {
        (0..128 * 128)
            .map(|i| {
                [
                    ((i % 128) as f32 + 0.5) / 128.,
                    ((i / 128) as f32 + 0.5) / 128.,
                    j as f32 * 0.2 + 0.3,
                    1.,
                ]
            })
            .collect()
    });
    let candidate = Warp::fit(&alignment(false), lens.clone())?;
    let direct = candidate.render([&buffers[0], &buffers[1]], 256, 128)?;
    let mut filter = Temporal::new(lens.clone())?;
    filter.update(candidate, 1. / 24.)?;
    let mut map = super::super::DualMap::new(lens, 256, 128)?;
    assert!(map.set_view_warp(crate::rotation::Rotation::IDENTITY, &filter)? > 0);
    let mut base = crate::pixels(256, 128)?;
    assert_eq!(map.render([&buffers[0], &buffers[1]], &mut base)?, 0);
    for (a, b) in base.iter().zip(&direct.stitched.pixels) {
        for c in 0..4 {
            assert!((a[c] - b[c]).abs() < 1e-5);
        }
    }
    let yaw = crate::rotation::Rotation::from_vector([0., std::f64::consts::FRAC_PI_2, 0.])?;
    map.set_view_warp(yaw, &filter)?;
    let mut rotated = crate::pixels(256, 128)?;
    assert_eq!(map.render([&buffers[0], &buffers[1]], &mut rotated)?, 0);
    for y in 0..128 {
        for x in 0..256 {
            let a = rotated[y * 256 + x];
            let b = base[y * 256 + (x + 64) % 256];
            for c in 0..4 {
                assert!((a[c] - b[c]).abs() < 1e-5, "x={x} y={y} c={c}");
            }
        }
    }
    Ok(())
}

fn lenses(size: u32) -> [MeiLens; 2] {
    std::array::from_fn(|j| MeiLens {
        width: size,
        height: size,
        xi: 1.,
        focal: [f64::from(size) * 0.42; 2],
        center: [f64::from(size) * 0.5; 2],
        radial: [0.; 3],
        tangential: [0.; 2],
        max_theta_degrees: 100.,
        world_to_lens: if j == 0 {
            [[1., 0., 0.], [0., -1., 0.], [0., 0., 1.]]
        } else {
            [[-1., 0., 0.], [0., -1., 0.], [0., 0., -1.]]
        },
    })
}
fn alignment(opposite_validation: bool) -> Alignment {
    Alignment {
        width: WIDTH,
        height: HEIGHT,
        vertical_span_degrees: SPAN,
        candidates: 3072,
        matches: (0..WIDTH)
            .step_by(64)
            .flat_map(|x| {
                (40..=88).step_by(8).map(move |y| {
                    let sign = if opposite_validation && (x / 8 + y / 8) % 4 == 0 {
                        -1.
                    } else {
                        1.
                    };
                    Match {
                        first: [x, y],
                        second: [
                            (x as f64 + 2. * sign).rem_euclid(WIDTH as f64),
                            y as f64 - 3. * sign,
                        ],
                        correlation: 0.99,
                        cycle_error_pixels: 0.,
                        first_rgb: None,
                        second_rgb: None,
                    }
                })
            })
            .collect(),
    }
}
#[test]
fn held_out_gate_wrap_and_rejected_export_are_exact() -> Result<()> {
    let lenses = lenses(64);
    let fitted = Warp::fit(&alignment(false), lenses.clone())?;
    assert!(fitted.report().accepted, "{:?}", fitted.report());
    assert!(fitted.report().maximum_field_gradient <= 0.50000001);
    assert!(fitted.report().minimum_strip_jacobian >= 0.56249999);
    assert_eq!(
        interpolate(&fitted.field, [0., 64.]),
        interpolate(&fitted.field, [WIDTH as f64, 64.])
    );
    for (a, b) in interpolate(&fitted.field, [0., 64.]).iter().zip([2., -3.]) {
        assert!((a - b).abs() < 1e-9);
    }
    let rejected = Warp::fit(&alignment(true), lenses.clone())?;
    assert!(!rejected.report().accepted);
    assert_eq!(
        rejected.report().reason,
        "held_out_alignment_did_not_improve"
    );
    let sources = [
        vec![[0.2, 0.3, 0.4, 1.]; 64 * 64],
        vec![[0.4, 0.2, 0.3, 1.]; 64 * 64],
    ];
    let inputs = [
        Input {
            lens: &lenses[0],
            pixels: &sources[0],
        },
        Input {
            lens: &lenses[1],
            pixels: &sources[1],
        },
    ];
    let baseline = super::super::stitch(&inputs, 128, 64)?;
    let result = rejected.render([&sources[0], &sources[1]], 128, 64)?;
    assert_eq!(result.warped_pixels, 0);
    assert_eq!(result.stitched.pixels, baseline.pixels);
    let mut invalid = alignment(false);
    invalid.matches.push(invalid.matches[0].clone());
    assert!(Warp::fit(&invalid, lenses.clone()).is_err());
    invalid.matches.pop();
    invalid.matches[0].second[1] = f64::NAN;
    assert!(Warp::fit(&invalid, lenses.clone()).is_err());
    invalid.matches.clear();
    assert!(!Warp::fit(&invalid, lenses)?.report().accepted);
    Ok(())
}
#[test]
fn gradient_bound_prevents_strip_folding_between_nodes() {
    let mut field = vec![[0.; 2]; GX * GY];
    field[8 * GX] = [7.5, -7.5];
    let scale = 0.5 / derivatives(&field).0;
    for f in &mut field {
        for v in f {
            *v *= scale;
        }
    }
    for x in (-4..16).map(f64::from) {
        for y in (56..73).map(f64::from) {
            let p = interpolate(&field, [x + 0.3, y + 0.2]);
            let q = interpolate(&field, [x + 0.301, y + 0.2]);
            let r = interpolate(&field, [x + 0.3, y + 0.201]);
            let dx = [(q[0] - p[0]) / 0.001, (q[1] - p[1]) / 0.001];
            let dy = [(r[0] - p[0]) / 0.001, (r[1] - p[1]) / 0.001];
            for sign in [-0.5, 0.5] {
                let determinant =
                    (1. + sign * dx[0]) * (1. + sign * dy[1]) - sign * sign * dx[1] * dy[0];
                assert!((0.56249..=1.56251).contains(&determinant));
            }
        }
    }
}
#[test]
fn direct_source_warp_reduces_known_nonconstant_seam_error() -> Result<()> {
    let lenses = lenses(512);
    let warp = Warp::fit(&alignment(false), lenses.clone())?;
    assert!(warp.report().accepted);
    let scene = |phi: f64, beta: f64| {
        (0.4 + 0.1 * (phi * 50.).sin() + 0.1 * (beta * 150. + phi * 17.).cos()) as f32
    };
    let buffers: [Vec<[f32; 4]>; 2] = std::array::from_fn(|j| {
        (0..512 * 512)
            .map(|i| {
                let x = ((i % 512) as f64 + 0.5 - 256.) / (512. * 0.42);
                let y = ((i / 512) as f64 + 0.5 - 256.) / (512. * 0.42);
                let r = x * x + y * y;
                let local = [2. * x / (1. + r), 2. * y / (1. + r), (1. - r) / (1. + r)];
                let world = std::array::from_fn(|i| {
                    (0..3)
                        .map(|k| lenses[j].world_to_lens[k][i] * local[k])
                        .sum()
                });
                let axis = rotate(lenses[0].world_to_lens, world);
                let phi = axis[1].atan2(axis[0]);
                let beta = axis[2].clamp(-1., 1.).asin();
                // Independently construct two translated observations of one angular scene.
                let sign = if j == 0 { 1. } else { -1. };
                let value = scene(
                    phi + sign * std::f64::consts::TAU / WIDTH as f64,
                    beta + sign * 1.5 * SPAN.to_radians() / HEIGHT as f64,
                );
                [value, value, value, 1.]
            })
            .collect()
    });
    let inputs = [
        Input {
            lens: &lenses[0],
            pixels: &buffers[0],
        },
        Input {
            lens: &lenses[1],
            pixels: &buffers[1],
        },
    ];
    let baseline = super::super::stitch(&inputs, 1024, 512)?;
    let output = warp.render([&buffers[0], &buffers[1]], 1024, 512)?;
    assert_eq!(output.stitched.uncovered_pixels, 0);
    assert!(output.warped_pixels > 0);
    let (mut before, mut after, mut count) = (0., 0., 0usize);
    for (i, (p, b)) in output
        .stitched
        .pixels
        .iter()
        .zip(&baseline.pixels)
        .enumerate()
    {
        let d = direction(
            ((i % 1024) as f64 + 0.5) / 1024.,
            ((i / 1024) as f64 + 0.5) / 512.,
        );
        let axis = rotate(lenses[0].world_to_lens, d);
        let beta = axis[2].clamp(-1., 1.).asin();
        assert!((p[3] - 1.).abs() < 1e-6);
        if beta.abs() > 12f64.to_radians() {
            assert_eq!(p, b);
        }
        if beta.abs() < 1f64.to_radians() {
            let expected = scene(axis[1].atan2(axis[0]), beta);
            before += f64::from((b[0] - expected).abs());
            after += f64::from((p[0] - expected).abs());
            count += 1;
        }
    }
    assert!(count > 1000);
    assert!(
        after < before * 0.5,
        "before={before}, after={after}, count={count}"
    );
    Ok(())
}
