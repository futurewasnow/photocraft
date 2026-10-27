use sphere_core::{Panorama, finish::*};

/// Original per-pixel 3D tangent construction, retained as an optimization oracle.
/// Explicit sigma isolates geometry/filtering from the unchanged noise estimator.
fn reference_denoise(w: u32, h: u32, data: &[[f32; 4]], radius_pixels: f64) -> Vec<[f32; 4]> {
    use std::f64::consts::{PI, TAU};
    let source = Panorama::new(w, h, data).unwrap();
    let radius = radius_pixels * TAU / f64::from(w);
    let sigma = f64::from(0.03f32);
    let strength = f64::from(0.8f32);
    let range_scale = 1.0 / (2.0 * 3.0 * (2.0 * sigma).powi(2));
    data.iter()
        .enumerate()
        .map(|(i, center)| {
            if center[3] < 1e-6 {
                return *center;
            }
            let straight = center.map(|v| f64::from(v) / f64::from(center[3]));
            let u = ((i % w as usize) as f64 + 0.5) / f64::from(w);
            let v = ((i / w as usize) as f64 + 0.5) / f64::from(h);
            let d = sphere_core::direction(u, v);
            let (sl, cl) = ((u - 0.5) * TAU).sin_cos();
            let east = [cl, 0.0, -sl];
            let north = [-d[1] * sl, (1.0 - d[1] * d[1]).max(0.0).sqrt(), -d[1] * cl];
            let mut sum = [straight[0], straight[1], straight[2]];
            let mut total = 1.0;
            for ring in 1..=2 {
                for k in 0..12 {
                    let r = radius * f64::from(ring) / 2.0;
                    let a = f64::from(k) * TAU / 12.0;
                    let (dc, ec, nc) = (r.cos(), r.sin() * a.cos(), r.sin() * a.sin());
                    let direction: [f64; 3] =
                        std::array::from_fn(|j| d[j] * dc + east[j] * ec + north[j] * nc);
                    let sample = source
                        .sample_uv(
                            direction[0].atan2(direction[2]) / TAU + 0.5,
                            0.5 - direction[1].clamp(-1.0, 1.0).asin() / PI,
                        )
                        .unwrap();
                    if sample[3] < 1e-6 || (sample[3] - center[3]).abs() > 0.05 {
                        continue;
                    }
                    let s = sample.map(|v| f64::from(v) / f64::from(sample[3]));
                    let distance = (0..3).map(|c| (s[c] - straight[c]).powi(2)).sum::<f64>();
                    let weight =
                        (-0.5 * f64::from(ring * ring)).exp() * (-distance * range_scale).exp();
                    for c in 0..3 {
                        sum[c] += s[c] * weight;
                    }
                    total += weight;
                }
            }
            let mut out = *center;
            for c in 0..3 {
                out[c] = ((straight[c] * (1.0 - strength) + sum[c] / total * strength)
                    * f64::from(center[3])) as f32;
            }
            out
        })
        .collect()
}

#[test]
fn denoise_matches_original_tangent_geometry_across_rows_and_poles() {
    for (w, h) in [(2, 1), (32, 16), (130, 65)] {
        let (_, mut data) = noisy_edge(w, h);
        for (i, p) in data.iter_mut().enumerate() {
            if i % 13 == 0 {
                *p = [0.0; 4];
            } else if i % 7 == 0 {
                *p = p.map(|v| v * 0.5);
            } else if i % 5 == 0 {
                p[0] += 1.5;
                p[1] -= 0.4;
            }
        }
        for radius_pixels in [0.5, 1.5, 3.0] {
            let expected = reference_denoise(w, h, &data, radius_pixels);
            let actual = denoise(
                &Panorama::new(w, h, &data).unwrap(),
                DenoiseOptions {
                    sigma: Some(0.03),
                    radius_pixels,
                    ..Default::default()
                },
            )
            .unwrap();
            for (i, (a, b)) in actual.pixels.iter().zip(expected).enumerate() {
                assert_eq!(a[3], b[3]);
                for c in 0..3 {
                    assert!(
                        (a[c] - b[c]).abs() < 0.000002,
                        "{w}x{h}, radius {radius_pixels}, pixel {i}, channel {c}: {} vs {}",
                        a[c],
                        b[c]
                    );
                }
            }
        }
    }
}

#[test]
fn controlled_processing_cancels_without_changing_inputs() {
    let (_, data) = noisy_edge(256, 128);
    let original = data.clone();
    let source = Panorama::new(256, 128, &data).unwrap();
    assert!(matches!(
        denoise_with_control(&source, DenoiseOptions::default(), &|_| false),
        Err(sphere_core::Error::Cancelled)
    ));
    assert!(matches!(
        denoise_with_control(&source, DenoiseOptions::default(), &|fraction| fraction
            < 0.02),
        Err(sphere_core::Error::Cancelled)
    ));
    assert!(matches!(
        denoise_with_control(&source, DenoiseOptions::default(), &|fraction| fraction
            < 0.5),
        Err(sphere_core::Error::Cancelled)
    ));
    let plate_data = vec![[0.5, 0.5, 0.5, 1.0]; 64 * 64];
    let plate = Plate {
        width: 64,
        height: 64,
        pixels: &plate_data,
        horizontal_fov: 90.0,
    };
    assert!(matches!(
        patch_pole_with_control(&source, &plate, PolePatch::default(), &|fraction| fraction
            < 0.5),
        Err(sphere_core::Error::Cancelled)
    ));
    assert_eq!(data, original);
}

fn noisy_edge(w: u32, h: u32) -> (Vec<[f32; 4]>, Vec<[f32; 4]>) {
    let mut state = 19u64;
    let mut uniform = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (((state >> 32) as f64) + 0.5) / 4294967296.0
    };
    let clean: Vec<_> = (0..w * h)
        .map(|i| {
            if i % w < w / 2 {
                [0.2, 0.3, 0.25, 1.0]
            } else {
                [0.75, 0.7, 0.8, 1.0]
            }
        })
        .collect();
    let noisy = clean
        .iter()
        .map(|p| {
            let mut q = *p;
            for v in &mut q[..3] {
                let n = (-2.0 * uniform().ln()).sqrt() * (std::f64::consts::TAU * uniform()).cos();
                *v += 0.025 * n as f32;
            }
            q
        })
        .collect();
    (clean, noisy)
}

#[test]
fn automatic_denoise_reduces_known_noise_without_erasing_edges() {
    let (clean, noisy) = noisy_edge(256, 128);
    let src = Panorama::new(256, 128, &noisy).unwrap();
    let result = denoise(&src, DenoiseOptions::default()).unwrap();
    assert!(result.report.applied);
    assert!((0.020..0.030).contains(&result.report.sigma));
    let mse = |data: &[[f32; 4]]| -> f64 {
        data.iter()
            .zip(&clean)
            .map(|(a, b)| (0..3).map(|c| f64::from(a[c] - b[c]).powi(2)).sum::<f64>())
            .sum::<f64>()
            / clean.len() as f64
            / 3.0
    };
    assert!(mse(&result.pixels) < mse(&noisy) * 0.65);
    // Both the ordinary image edge and the wrap edge are real scene edges here.
    for x in [0, 127, 128, 255] {
        let mean = (16..112)
            .map(|y| result.pixels[y * 256 + x][0])
            .sum::<f32>()
            / 96.0;
        let expected = if x < 128 { 0.2 } else { 0.75 };
        assert!((mean - expected).abs() < 0.012, "edge {x}: {mean}");
    }
    assert!(result.pixels.iter().all(|p| p[3] == 1.0));
}

#[test]
fn denoise_is_yaw_equivariant_and_preserves_transparency() {
    let (_, mut data) = noisy_edge(64, 32);
    data[200] = [0.0; 4];
    let shifted: Vec<_> = (0..data.len())
        .map(|i| data[i / 64 * 64 + (i % 64 + 17) % 64])
        .collect();
    let options = DenoiseOptions {
        sigma: Some(0.025),
        ..Default::default()
    };
    let a = denoise(&Panorama::new(64, 32, &data).unwrap(), options).unwrap();
    let b = denoise(&Panorama::new(64, 32, &shifted).unwrap(), options).unwrap();
    for (i, p) in b.pixels.iter().enumerate() {
        let q = a.pixels[i / 64 * 64 + (i % 64 + 17) % 64];
        for c in 0..4 {
            assert!((p[c] - q[c]).abs() < 2e-6);
        }
    }
    assert_eq!(a.pixels[200], [0.0; 4]);
}

#[test]
fn clean_and_unreliable_inputs_do_not_get_automatic_blur() {
    for (w, h) in [(64, 32), (8, 4)] {
        let data = vec![[0.2, 0.3, 0.4, 1.0]; (w * h) as usize];
        let result = denoise(
            &Panorama::new(w, h, &data).unwrap(),
            DenoiseOptions::default(),
        )
        .unwrap();
        assert!(!result.report.applied);
        assert_eq!(result.pixels, data);
    }
}

#[test]
fn pole_plate_fills_only_selected_cap_and_detects_remaining_holes() {
    let mut data = vec![[0.2, 0.3, 0.4, 1.0]; 128 * 64];
    for p in &mut data[..128 * 4] {
        *p = [0.0; 4];
    }
    for p in &mut data[128 * 60..] {
        *p = [0.0; 4];
    }
    let source = Panorama::new(128, 64, &data).unwrap();
    let report = pole_coverage(&source, 20.0).unwrap();
    assert_eq!(report.zenith.missing_pixels, 512);
    assert_eq!(report.nadir.missing_pixels, 512);
    let plate_data = vec![[0.8, 0.1, 0.2, 1.0]; 64 * 64];
    let plate = Plate {
        width: 64,
        height: 64,
        pixels: &plate_data,
        horizontal_fov: 90.0,
    };
    let out = patch_pole(
        &source,
        &plate,
        PolePatch {
            pole: Pole::Zenith,
            radius_degrees: 20.0,
            feather_degrees: 5.0,
            yaw: 0.0,
        },
    )
    .unwrap();
    assert!(
        out[..128 * 4]
            .iter()
            .all(|p| (p[3] - 1.0).abs() < 1e-6 && (p[0] - 0.8).abs() < 1e-6)
    );
    assert_eq!(out[128 * 8..], data[128 * 8..]);
    let remaining = pole_coverage(&Panorama::new(128, 64, &out).unwrap(), 20.0).unwrap();
    assert_eq!(remaining.zenith.missing_pixels, 0);
    assert_eq!(remaining.nadir.missing_pixels, 512);
    // Zero-alpha donor must not destroy covered source pixels.
    let empty = vec![[0.0; 4]; 64 * 64];
    let transparent = Plate {
        pixels: &empty,
        ..plate
    };
    assert_eq!(
        patch_pole(&source, &transparent, PolePatch::default()).unwrap(),
        data
    );
}

#[test]
fn patch_roundtrip_matches_rectilinear_orientation_at_both_poles() {
    let data: Vec<_> = (0..256 * 128)
        .map(|i| {
            let d = sphere_core::direction(
                (i % 256) as f64 / 256.0 + 0.5 / 256.0,
                (i / 256) as f64 / 128.0 + 0.5 / 128.0,
            );
            [
                (d[0] * 0.4 + 0.5) as f32,
                (d[1] * 0.4 + 0.5) as f32,
                (d[2] * 0.4 + 0.5) as f32,
                1.0,
            ]
        })
        .collect();
    let source = Panorama::new(256, 128, &data).unwrap();
    for (pole, pitch) in [(Pole::Zenith, 90.0), (Pole::Nadir, -90.0)] {
        let plate_data = sphere_core::perspective(
            &source,
            128,
            96,
            90.0,
            sphere_core::Orientation {
                yaw: 37.0,
                pitch,
                roll: 0.0,
            },
        )
        .unwrap();
        let plate = Plate {
            width: 128,
            height: 96,
            pixels: &plate_data,
            horizontal_fov: 90.0,
        };
        let out = patch_pole(
            &source,
            &plate,
            PolePatch {
                pole,
                yaw: 37.0,
                ..Default::default()
            },
        )
        .unwrap();
        for (a, b) in out.iter().zip(&data) {
            for c in 0..4 {
                assert!((a[c] - b[c]).abs() < 0.0003);
            }
        }
    }
}

#[test]
fn invalid_parameters_and_pixels_are_rejected_before_processing() {
    let mut data = vec![[0.2, 0.3, 0.4, 1.0]; 128];
    let source = Panorama::new(16, 8, &data).unwrap();
    assert!(
        denoise(
            &source,
            DenoiseOptions {
                sigma: Some(f32::NAN),
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(
        denoise(
            &source,
            DenoiseOptions {
                radius_pixels: 1000.0,
                ..Default::default()
            }
        )
        .is_err()
    );
    let plate = Plate {
        width: 16,
        height: 8,
        pixels: &data,
        horizontal_fov: 20.0,
    };
    assert!(patch_pole(&source, &plate, PolePatch::default()).is_err());
    assert!(pole_coverage(&source, f64::NAN).is_err());
    data[30][0] = f32::INFINITY;
    assert!(
        denoise(
            &Panorama::new(16, 8, &data).unwrap(),
            DenoiseOptions::default()
        )
        .is_err()
    );
}
