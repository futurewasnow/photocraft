use sphere_core::{Orientation, Panorama, blur, brush_weight, orient, perspective};
fn fixture(w: u32, h: u32) -> Vec<[f32; 4]> {
    (0..w * h)
        .map(|i| {
            let d = sphere_core::direction(
                (f64::from(i % w) + 0.5) / f64::from(w),
                (f64::from(i / w) + 0.5) / f64::from(h),
            );
            [
                (d[0] * 0.5 + 0.5) as f32,
                (d[1] * 0.5 + 0.5) as f32,
                (d[2] * 0.5 + 0.5) as f32,
                1.0,
            ]
        })
        .collect()
}
#[test]
fn identity_retains_pixel_centres() {
    let data = fixture(64, 32);
    let p = Panorama::new(64, 32, &data).unwrap();
    let out = orient(&p, Orientation::default()).unwrap();
    for (a, b) in data.iter().zip(out) {
        for c in 0..4 {
            assert!((a[c] - b[c]).abs() < 1e-6);
        }
    }
}
#[test]
fn horizontal_wrap_and_pole_reflection() {
    let data = fixture(64, 32);
    let p = Panorama::new(64, 32, &data).unwrap();
    assert_eq!(
        p.sample_uv(0.0, 0.5).unwrap(),
        p.sample_uv(1.0, 0.5).unwrap()
    );
    assert_eq!(
        p.sample_uv(0.2, -0.1).unwrap(),
        p.sample_uv(0.7, 0.1).unwrap()
    );
    assert_eq!(
        p.sample_uv(0.2, 1.1).unwrap(),
        p.sample_uv(0.7, 0.9).unwrap()
    );
}
#[test]
fn angular_brush_crosses_seam_and_poles() {
    let a = brush_weight(0.99, 0.5, 0.0, 0.5, 20.0).unwrap();
    let b = brush_weight(0.01, 0.5, 0.0, 0.5, 20.0).unwrap();
    assert!(a > 0.8);
    assert!((a - b).abs() < 1e-6);
    assert!((brush_weight(0.1, 0.0, 0.9, 0.0, 1.0).unwrap() - 1.0).abs() < 1e-6);
}
#[test]
fn rotation_full_turn_and_direction() {
    let data = fixture(64, 32);
    let p = Panorama::new(64, 32, &data).unwrap();
    let out = orient(
        &p,
        Orientation {
            yaw: 360.0,
            ..Default::default()
        },
    )
    .unwrap();
    for (a, b) in data.iter().zip(out) {
        for c in 0..4 {
            assert!((a[c] - b[c]).abs() < 1e-6);
        }
    }
    let view = perspective(
        &p,
        1,
        1,
        90.0,
        Orientation {
            yaw: 90.0,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(view[0][0] > 0.99);
    assert!((view[0][2] - 0.5).abs() < 0.01);
}
#[test]
fn blur_preserves_constant_hdr_alpha() {
    let data = vec![[4.0, 2.0, 1.0, 0.5]; 128];
    let p = Panorama::new(16, 8, &data).unwrap();
    for pixel in blur(&p, 10.0).unwrap() {
        for (a, b) in pixel.iter().zip(data[0]) {
            assert!((a - b).abs() < 1e-5);
        }
    }
}
#[test]
fn reject_hostile_inputs() {
    assert!(Panorama::new(0, 0, &[]).is_err());
    assert!(Panorama::new(4, 2, &[]).is_err());
    assert!(sphere_core::pixels(u32::MAX, u32::MAX).is_err());
    let data = fixture(4, 2);
    let p = Panorama::new(4, 2, &data).unwrap();
    assert!(p.sample_uv(f64::NAN, 0.0).is_err());
    assert!(
        orient(
            &p,
            Orientation {
                yaw: f64::INFINITY,
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(blur(&p, -1.0).is_err());
    assert!(perspective(&p, 10, 10, 180.0, Orientation::default()).is_err());
}
#[test]
fn dual_fisheye_covers_full_sphere_and_reports_holes() {
    use sphere_core::stitch::{Camera, Input, Lens, stitch};
    let a = Camera {
        width: 32,
        height: 32,
        lens: Lens::EquidistantFisheye,
        fov: 200.0,
        orientation: Orientation::default(),
        center: [0.5, 0.5],
        radius_pixels: None,
        exposure: 0.0,
    };
    let b = Camera {
        orientation: Orientation {
            yaw: 180.0,
            ..Default::default()
        },
        ..a.clone()
    };
    let data = vec![[0.25, 0.5, 2.0, 1.0]; 1024];
    let out = stitch(
        &[
            Input {
                camera: &a,
                pixels: &data,
            },
            Input {
                camera: &b,
                pixels: &data,
            },
        ],
        64,
        32,
    )
    .unwrap();
    assert_eq!(out.uncovered_pixels, 0);
    for p in out.pixels {
        assert!((p[2] - 2.0).abs() < 1e-6);
    }
    let partial = stitch(
        &[Input {
            camera: &a,
            pixels: &data,
        }],
        64,
        32,
    )
    .unwrap();
    assert!(partial.uncovered_pixels > 0);
}
#[test]
fn jpeg_metadata_is_sized_and_full_sphere_only() {
    let jpeg = [0xff, 0xd8, 0xff, 0xd9];
    let out = sphere_core::metadata::tag_new_jpeg(&jpeg, 4000, 2000).unwrap();
    let n = u16::from_be_bytes([out[4], out[5]]) as usize;
    assert_eq!(&out[4 + n..], &[0xff, 0xd9]);
    assert!(String::from_utf8_lossy(&out).contains("equirectangular"));
    assert!(sphere_core::metadata::gpano(2000, 2000).is_err());
}

#[test]
fn calibrated_nonconstant_rig_reconstructs_directions() {
    use sphere_core::stitch::{Camera, Input, Lens, stitch};
    let w = 128;
    let h = 128;
    let lens = Camera {
        width: w,
        height: h,
        lens: Lens::EquidistantFisheye,
        fov: 200.0,
        orientation: Orientation::default(),
        center: [0.5, 0.5],
        radius_pixels: None,
        exposure: 0.0,
    };
    let back = Camera {
        orientation: Orientation {
            yaw: 180.0,
            ..Default::default()
        },
        ..lens.clone()
    };
    let lens_image = |back: bool| {
        (0..w * h)
            .map(|i| {
                let x = 2.0 * (f64::from(i % w) + 0.5) / f64::from(w) - 1.0;
                let y = 1.0 - 2.0 * (f64::from(i / w) + 0.5) / f64::from(h);
                let r = x.hypot(y);
                if r > 1.0 {
                    return [0.0; 4];
                }
                let theta = r * 100.0f64.to_radians();
                let d = if r < 1e-12 {
                    [0.0, 0.0, 1.0]
                } else {
                    [theta.sin() * x / r, theta.sin() * y / r, theta.cos()]
                };
                let d = if back { [-d[0], d[1], -d[2]] } else { d };
                [
                    (0.5 + 0.5 * d[0]) as f32,
                    (0.5 + 0.5 * d[1]) as f32,
                    (0.5 + 0.5 * d[2]) as f32,
                    1.0,
                ]
            })
            .collect::<Vec<_>>()
    };
    let front_data = lens_image(false);
    let back_data = lens_image(true);
    let stitched = stitch(
        &[
            Input {
                camera: &lens,
                pixels: &front_data,
            },
            Input {
                camera: &back,
                pixels: &back_data,
            },
        ],
        128,
        64,
    )
    .unwrap();
    assert_eq!(stitched.uncovered_pixels, 0);
    let truth = fixture(128, 64);
    let error: f32 = truth
        .iter()
        .zip(&stitched.pixels)
        .map(|(a, b)| (0..3).map(|j| (a[j] - b[j]).abs()).sum::<f32>())
        .sum::<f32>()
        / (128.0 * 64.0 * 3.0);
    assert!(error < 0.001, "mean direction reconstruction error {error}");
    // The same circle inside an offset rectangular sensor crop must reconstruct identically.
    let padded_camera = Camera {
        width: 192,
        height: 160,
        center: [80.0 / 192.0, 88.0 / 160.0],
        radius_pixels: Some(64.0),
        ..lens.clone()
    };
    let padded_back = Camera {
        orientation: back.orientation,
        ..padded_camera.clone()
    };
    let pad = |source: &[[f32; 4]]| {
        let mut image = vec![[19.0, 1.0, 0.0, 1.0]; 192 * 160];
        for y in 0..128 {
            for x in 0..128 {
                image[(y + 24) * 192 + x + 16] = source[y * 128 + x];
            }
        }
        image
    };
    let (padded_front_data, padded_back_data) = (pad(&front_data), pad(&back_data));
    let padded = stitch(
        &[
            Input {
                camera: &padded_camera,
                pixels: &padded_front_data,
            },
            Input {
                camera: &padded_back,
                pixels: &padded_back_data,
            },
        ],
        128,
        64,
    )
    .unwrap();
    assert_eq!(padded.uncovered_pixels, 0);
    assert!(
        padded
            .pixels
            .iter()
            .zip(&stitched.pixels)
            .all(|(a, b)| (0..4).all(|c| (a[c] - b[c]).abs() < 1e-5))
    );
    for radius in [f64::NAN, f64::INFINITY, -1.0, 0.0, 100000.0] {
        assert!(
            Camera {
                radius_pixels: Some(radius),
                ..lens.clone()
            }
            .validate()
            .is_err()
        );
    }
    let transparent = vec![[0.0; 4]; (w * h) as usize];
    let empty = stitch(
        &[
            Input {
                camera: &lens,
                pixels: &transparent,
            },
            Input {
                camera: &back,
                pixels: &transparent,
            },
        ],
        128,
        64,
    )
    .unwrap();
    assert_eq!(empty.uncovered_pixels, 128 * 64);
}
