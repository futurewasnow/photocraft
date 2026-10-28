use sphere_core::{Error, Panorama, finish::*};

fn fixture(count: usize) -> Vec<[f32; 4]> {
    (0..count)
        .map(|i| {
            let alpha = [0.0, 0.25, 0.75, 1.0][i % 4];
            [
                ((i % 17) as f32 - 5.0) * alpha,
                0.3 * alpha,
                1.7 * alpha,
                alpha,
            ]
        })
        .collect()
}

#[test]
fn banded_and_cap_only_repairs_match_full_frame_exactly() {
    let donor = fixture(64 * 48);
    let plate = Plate {
        width: 64,
        height: 48,
        pixels: &donor,
        horizontal_fov: 150.0,
    };
    for (width, height) in [(2, 1), (6, 3), (64, 32), (130, 65)] {
        let input = fixture((width * height) as usize);
        let source = Panorama::new(width, height, &input).unwrap();
        for pole in [Pole::Zenith, Pole::Nadir] {
            for yaw in [-179.9, 0.0, 37.0, 180.0] {
                for radius in [0.1, 20.0, 60.0] {
                    let patch = PolePatch {
                        pole,
                        yaw,
                        radius_degrees: radius,
                        feather_degrees: radius / 4.0,
                    };
                    let expected =
                        reference_patch_pole(width, height, &input, &plate, patch).unwrap();
                    assert_eq!(patch_pole(&source, &plate, patch).unwrap(), expected);
                    let prepared = PreparedPolePatch::new(&plate, patch, width, height).unwrap();
                    for band_rows in [1, 7, height] {
                        let mut actual = Vec::new();
                        for (index, band) in input.chunks((width * band_rows) as usize).enumerate()
                        {
                            actual.extend(
                                prepared
                                    .apply_band(index as u32 * band_rows, band, &|_| true)
                                    .unwrap(),
                            );
                        }
                        assert_eq!(
                            actual, expected,
                            "{width}x{height} {pole:?} yaw {yaw} radius {radius}"
                        );
                    }
                    let range = prepared.affected_rows();
                    let start = (range.start * width) as usize;
                    let end = (range.end * width) as usize;
                    let mut cap_only = input.clone();
                    cap_only[start..end].copy_from_slice(
                        &prepared
                            .apply_band(range.start, &input[start..end], &|_| true)
                            .unwrap(),
                    );
                    assert_eq!(cap_only, expected);
                }
            }
        }
    }
}

#[test]
fn band_errors_and_cancellation_leave_inputs_unchanged() {
    let donor = fixture(64 * 48);
    let plate = Plate {
        width: 64,
        height: 48,
        pixels: &donor,
        horizontal_fov: 90.0,
    };
    let prepared = PreparedPolePatch::new(&plate, PolePatch::default(), 64, 32).unwrap();
    let input = fixture(64 * 8);
    let original = input.clone();
    for (first_row, band) in [
        (0, &input[..0]),
        (0, &input[..63]),
        (32, &input[..64]),
        (u32::MAX, &input[..64]),
        (25, input.as_slice()),
    ] {
        assert!(prepared.apply_band(first_row, band, &|_| true).is_err());
    }
    for threshold in [0.0, 0.25, 1.0] {
        assert!(matches!(
            prepared.apply_band(24, &input, &|p| p < threshold),
            Err(Error::Cancelled)
        ));
    }
    let mut invalid = input.clone();
    invalid[0][0] = f32::NAN;
    assert!(prepared.apply_band(24, &invalid, &|_| true).is_err());
    assert_eq!(input, original);
    for (width, height) in [(0, 0), (64, 0), (63, 32), (u32::MAX, u32::MAX)] {
        assert!(PreparedPolePatch::new(&plate, PolePatch::default(), width, height).is_err());
    }
    assert!(
        PreparedPolePatch::new(
            &plate,
            PolePatch {
                yaw: f64::NAN,
                ..Default::default()
            },
            64,
            32
        )
        .is_err()
    );
}

#[test]
fn huge_global_sphere_uses_only_a_single_row_buffer() {
    // A full 65536 x 32768 RGBA32F frame would require 32 GiB.
    let donor = vec![[0.8, 0.4, 0.2, 1.0]; 64 * 64];
    let plate = Plate {
        width: 64,
        height: 64,
        pixels: &donor,
        horizontal_fov: 90.0,
    };
    let input = vec![[0.0; 4]; 65536];
    for pole in [Pole::Zenith, Pole::Nadir] {
        let prepared = PreparedPolePatch::new(
            &plate,
            PolePatch {
                pole,
                ..Default::default()
            },
            65536,
            32768,
        )
        .unwrap();
        let row = if pole == Pole::Zenith { 0 } else { 32767 };
        let output = prepared.apply_band(row, &input, &|_| true).unwrap();
        assert_eq!(output.len(), input.len());
        assert!(
            output
                .iter()
                .all(|p| (p[3] - 1.0).abs() < 1e-6 && (p[0] - 0.8).abs() < 1e-6)
        );
        assert_eq!(
            prepared.apply_band(16384, &input, &|_| true).unwrap(),
            input
        );
    }
}

// Frozen full-frame implementation predating row bands: independent numerical oracle.
fn reference_patch_pole(
    width: u32,
    height: u32,
    data: &[[f32; 4]],
    plate: &Plate<'_>,
    patch: PolePatch,
) -> sphere_core::Result<Vec<[f32; 4]>> {
    use sphere_core::{Orientation, direction, pixel_count, pixels};
    use std::f64::consts::PI;
    let control = |_: f32| true;
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
    let orientation = Orientation {
        yaw: patch.yaw,
        pitch,
        roll: 0.0,
    }
    .validate()?;
    let (sy, cy) = orientation.yaw.to_radians().sin_cos();
    let (sp, cp) = orientation.pitch.to_radians().sin_cos();
    let (sr, cr) = orientation.roll.to_radians().sin_cos();
    let m = [
        [cy * cr - sy * sp * sr, -cy * sr - sy * sp * cr, sy * cp],
        [cp * sr, cp * cr, sp],
        [-sy * cr - cy * sp * sr, sy * sr - cy * sp * cr, cy * cp],
    ];
    let mut out = pixels(width, height)?;
    let radius = patch.radius_degrees.to_radians();
    let feather = patch.feather_degrees.to_radians();
    out.iter_mut()
        .enumerate()
        .try_for_each(|(i, p)| -> sphere_core::Result<()> {
            if i.is_multiple_of(width as usize) && !control(i as f32 / data.len() as f32) {
                return Err(Error::Cancelled);
            }
            *p = data[i];
            let u = ((i % width as usize) as f64 + 0.5) / f64::from(width);
            let v = ((i / width as usize) as f64 + 0.5) / f64::from(height);
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
            *p = std::array::from_fn(|c| donor[c] + data[i][c] * (1.0 - donor[3]));
            Ok(())
        })?;
    if !control(1.0) {
        return Err(Error::Cancelled);
    }
    Ok(out)
}
