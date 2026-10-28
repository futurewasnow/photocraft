//! Native command acceptance for spherical finishing and supplied clean-plate repair.
use photocraft_cms::{Builtin, Intent, Transform};
use photocraft_color::{ColorMode, PixelFormat, SampleType};
use photocraft_doc::{Document, Layer};
use photocraft_engine::{Session, jobs::JobCtx};
use photocraft_geom::Size;
use serde_json::json;
use std::sync::Arc;

fn document(name: &str, width: u32, height: u32, depth: SampleType, mode: ColorMode, pixel: &[f32]) -> Document {
    let mut doc = Document::new(name, Size::new(width, height), mode, depth);
    let mut layer = Layer::raster(name, PixelFormat::new(mode, depth, true));
    for y in 0..height {
        for x in 0..width {
            layer.surface_mut().unwrap().write_pixel(x as i32, y as i32, pixel);
        }
    }
    doc.layers.push(layer);
    doc
}
fn bytes(s: &Session, index: usize) -> Vec<u8> {
    let state = &s.documents()[index];
    state.doc.layer(state.active_layer.unwrap()).unwrap().surface().unwrap().try_to_interleaved(state.doc.bounds()).unwrap()
}
fn pixel(surface: &photocraft_raster::Surface, x: i32, y: i32) -> Vec<f32> {
    let mut values = vec![0.0; surface.format().channels()];
    surface.read_pixel(x, y, &mut values);
    values
}
fn fixture(depth: SampleType) -> (Session, u64) {
    let mut s = Session::new();
    s.add_document(document("sphere", 64, 32, depth, ColorMode::Rgb, &[0.7, 0.1, 0.1, 0.0]), None);
    s.add_document(document("plate", 16, 16, depth, ColorMode::Rgb, &[0.1, 0.4, 0.8, 1.0]), None);
    let donor = s.active().unwrap().doc.id.0;
    assert!(s.set_active(0));
    (s, donor)
}

#[test]
fn multi_band_caps_match_full_frame_and_late_failure_is_atomic() {
    use photocraft_algo::sphere360::{
        Panorama,
        finish::{Plate, Pole, PolePatch, patch_pole},
    };
    for pole in [Pole::Nadir, Pole::Zenith] {
        let mut s = Session::new();
        let mut target = document("sphere", 1024, 512, SampleType::F32, ColorMode::Rgb, &[0.2, -0.1, 1.7, 1.0]);
        target.icc_profile = Some(Builtin::LinearSrgb.profile().to_bytes());
        let mut donor = document("plate", 64, 64, SampleType::F32, ColorMode::Rgb, &[0.8, 0.2, 0.3, 0.75]);
        donor.icc_profile = target.icc_profile.clone();
        s.add_document(target, None);
        s.add_document(donor, None);
        let donor_id = s.active().unwrap().doc.id.0;
        s.set_active(0);
        let original = bytes(&s, 0);
        let params = json!({"donor_document":donor_id,"horizontal_fov":150,"patch":{"pole":pole,"radius_degrees":60,"feather_degrees":15,"yaw":37}});
        let report = s.execute("sphere360.patchPole", params.clone()).unwrap();
        assert!(report["processed_rows"].as_u64().unwrap() > 128);
        assert!(report["linear_band_storage_bytes"].as_u64().unwrap() <= 16 * 1024 * 1024);
        let source = vec![[0.2, -0.1, 1.7, 1.0]; 1024 * 512];
        let plate_pixels = vec![[0.8 * 0.75, 0.2 * 0.75, 0.3 * 0.75, 0.75]; 64 * 64];
        let expected = patch_pole(
            &Panorama::new(1024, 512, &source).unwrap(),
            &Plate { width: 64, height: 64, pixels: &plate_pixels, horizontal_fov: 150.0 },
            PolePatch { pole, radius_degrees: 60.0, feather_degrees: 15.0, yaw: 37.0 },
        )
        .unwrap();
        let state = s.active().unwrap();
        let actual = state.doc.layer(state.active_layer.unwrap()).unwrap().surface().unwrap().read_region(state.doc.bounds());
        for (a, b) in actual.as_chunks::<4>().0.iter().zip(&expected) {
            for c in 0..4 {
                assert!((a[c] - b[c]).abs() < 2e-6);
            }
        }
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(bytes(&s, 0), original);
        s.edit("invalid later band", |doc, active| {
            let y = if pole == Pole::Zenith { 170 } else { 511 };
            doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap().write_pixel(0, y, &[f32::NAN, 0.0, 0.0, 1.0]);
            Ok(())
        })
        .unwrap();
        let before = s.active().unwrap().doc.clone();
        assert!(s.execute("sphere360.patchPole", params).is_err());
        assert!(Arc::ptr_eq(&before, &s.active().unwrap().doc), "a later band failure must discard earlier staged edits");
    }
}

#[test]
fn both_poles_fill_holes_at_all_depths_and_restore_exactly() {
    for depth in SampleType::ALL {
        for pole in ["nadir", "zenith"] {
            let (mut s, donor) = fixture(depth);
            let original = bytes(&s, 0);
            let original_doc = s.active().unwrap().doc.clone();
            let donor_original = bytes(&s, 1);
            let result = s.execute("sphere360.patchPole", json!({"donor_document":donor,"horizontal_fov":90,"patch":{"pole":pole}})).unwrap();
            assert_eq!(result["width"], 64);
            assert_eq!(result["height"], 32);
            assert_eq!(bytes(&s, 1), donor_original);
            let output = bytes(&s, 0);
            assert_ne!(output, original);
            let state = s.active().unwrap();
            let surface = state.doc.layer(state.active_layer.unwrap()).unwrap().surface().unwrap();
            let y = if pole == "zenith" { 0 } else { 31 };
            for x in [0, 17, 32, 63] {
                let p = pixel(surface, x, y);
                assert!((p[3] - 1.0).abs() < 0.005, "{pole}: {p:?}");
                assert!((p[2] - 0.8).abs() < 0.005, "{pole}: {p:?}");
            }
            // Hidden native RGB outside the cap is unchanged, even with zero alpha.
            assert_eq!(pixel(surface, 0, 16)[3], 0.0);
            let untouched = photocraft_geom::Rect::new(0, 4, 64, 28);
            assert_eq!(surface.try_to_interleaved(untouched).unwrap(), original_doc.layers[0].surface().unwrap().try_to_interleaved(untouched).unwrap());
            s.execute("edit.undo", json!({})).unwrap();
            assert_eq!(bytes(&s, 0), original);
            s.execute("edit.redo", json!({})).unwrap();
            assert_eq!(bytes(&s, 0), output);
        }
    }
}

#[test]
fn background_repair_uses_donor_snapshot_and_keeps_other_tab_edits() {
    use photocraft_engine::jobs::Started;
    let (mut s, donor) = fixture(SampleType::F32);
    let original = bytes(&s, 0);
    let started = s.start("sphere360.patchPole", json!({"donor_document":donor,"horizontal_fov":90})).unwrap();
    let Started::Job(job) = started else { panic!("native repair must use a background job") };
    s.set_active(1);
    s.edit("new donor content", |doc, active| {
        let surface = doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap();
        surface.try_write_region(photocraft_geom::Rect::new(0, 0, 16, 16), &[1.0, 0.0, 0.0, 1.0].repeat(256)).unwrap();
        Ok(())
    })
    .unwrap();
    let edited_donor = bytes(&s, 1);
    s.wait_job(job).unwrap();
    assert_eq!(bytes(&s, 1), edited_donor);
    s.set_active(0);
    let state = s.active().unwrap();
    let surface = state.doc.layer(state.active_layer.unwrap()).unwrap().surface().unwrap();
    assert!((pixel(surface, 0, 31)[2] - 0.8).abs() < 1e-5);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(bytes(&s, 0), original);
    assert_eq!(bytes(&s, 1), edited_donor);
}

#[test]
fn bad_donor_profile_and_mask_leave_target_unchanged() {
    let (mut s, donor) = fixture(SampleType::F32);
    let before = s.active().unwrap().doc.clone();
    s.set_active(1);
    s.edit("bad profile", |doc, _| {
        doc.icc_profile = Some(Arc::new(vec![0; 128]));
        Ok(())
    })
    .unwrap();
    s.set_active(0);
    assert!(s.execute("sphere360.patchPole", json!({"donor_document":donor,"horizontal_fov":90})).is_err());
    assert!(Arc::ptr_eq(&before, &s.active().unwrap().doc));
    s.set_active(1);
    s.edit("masked donor", |doc, active| {
        doc.icc_profile = None;
        doc.layer_mut(active.unwrap()).unwrap().mask = Some(photocraft_doc::LayerMask::reveal_all());
        Ok(())
    })
    .unwrap();
    s.set_active(0);
    assert!(s.execute("sphere360.patchPole", json!({"donor_document":donor,"horizontal_fov":90})).is_err());
    assert!(Arc::ptr_eq(&before, &s.active().unwrap().doc));
}

#[test]
fn donor_profile_is_converted_independently_and_partial_alpha_composites() {
    use photocraft_algo::sphere360::{
        Panorama,
        finish::{Plate, PolePatch, patch_pole},
    };
    let mut s = Session::new();
    let mut target = document("linear target", 64, 32, SampleType::F32, ColorMode::Rgb, &[1.5, -0.02, 0.25, 0.4]);
    target.icc_profile = Some(Builtin::LinearSrgb.profile().to_bytes());
    let mut donor = document("sRGB plate", 16, 16, SampleType::F32, ColorMode::Rgb, &[0.5, 0.3, 0.9, 0.75]);
    donor.icc_profile = Some(Builtin::Srgb.profile().to_bytes());
    s.add_document(target, None);
    s.add_document(donor, None);
    let id = s.active().unwrap().doc.id.0;
    s.set_active(0);
    let donor_before = bytes(&s, 1);
    let input = vec![[1.5 * 0.4, -0.02 * 0.4, 0.25 * 0.4, 0.4]; 64 * 32];
    let transform = Transform::new(Builtin::Srgb.profile(), Builtin::LinearSrgb.profile(), Intent::RelativeColorimetric, false).unwrap();
    let mut linear = [0.0; 3];
    transform.eval(&[0.5, 0.3, 0.9], &mut linear);
    let plate = vec![[linear[0] * 0.75, linear[1] * 0.75, linear[2] * 0.75, 0.75]; 16 * 16];
    let expected =
        patch_pole(&Panorama::new(64, 32, &input).unwrap(), &Plate { width: 16, height: 16, pixels: &plate, horizontal_fov: 90.0 }, PolePatch::default())
            .unwrap();
    s.execute("sphere360.patchPole", json!({"donor_document":id,"horizontal_fov":90})).unwrap();
    let state = s.active().unwrap();
    let output = state.doc.layer(state.active_layer.unwrap()).unwrap().surface().unwrap().read_region(state.doc.bounds());
    for (p, expected) in output.as_chunks::<4>().0.iter().zip(expected) {
        assert!((p[3] - expected[3]).abs() < 1e-6);
        for c in 0..3 {
            assert!((p[c] * p[3] - expected[c]).abs() < 1e-5);
        }
    }
    assert_eq!(bytes(&s, 1), donor_before);
    assert_eq!(state.doc.icc_profile, Some(Builtin::LinearSrgb.profile().to_bytes()));
}

#[test]
fn grayscale_donor_and_target_retain_native_format() {
    for depth in SampleType::ALL {
        let mut s = Session::new();
        s.add_document(document("gray sphere", 32, 16, depth, ColorMode::Grayscale, &[0.2, 1.0]), None);
        s.add_document(document("gray donor", 16, 16, depth, ColorMode::Grayscale, &[0.6, 1.0]), None);
        let donor = s.active().unwrap().doc.id.0;
        s.set_active(0);
        s.execute("sphere360.patchPole", json!({"donor_document":donor,"horizontal_fov":90})).unwrap();
        let state = s.active().unwrap();
        let surface = state.doc.layer(state.active_layer.unwrap()).unwrap().surface().unwrap();
        assert_eq!(surface.format(), PixelFormat::new(ColorMode::Grayscale, depth, true));
        assert!((pixel(surface, 0, 15)[0] - 0.6).abs() < 0.005);
    }
}

#[test]
fn bad_geometry_missing_sources_locks_and_cancel_are_atomic() {
    let (mut s, donor) = fixture(SampleType::F32);
    let target = s.active().unwrap().doc.id.0;
    for params in [
        json!(null),
        json!({}),
        json!({"donor_document":donor,"horizontal_fov":90,"unknown":true}),
        json!({"donor_document":target,"horizontal_fov":90}),
        json!({"donor_document":u64::MAX,"horizontal_fov":90}),
        json!({"donor_document":donor,"horizontal_fov":90,"patch":{"radius_degrees":50}}),
        json!({"donor_document":donor,"horizontal_fov":90,"patch":{"feather_degrees":0}}),
        json!({"donor_document":donor,"horizontal_fov":0}),
        json!({"donor_document":donor,"horizontal_fov":1e100}),
    ] {
        let before = s.active().unwrap().doc.clone();
        assert!(s.execute("sphere360.patchPole", params).is_err());
        assert!(Arc::ptr_eq(&before, &s.active().unwrap().doc));
    }
    let ctx = JobCtx::new();
    ctx.cancel();
    s.set_inline_job_ctx(Some(ctx));
    let before = s.active().unwrap().doc.clone();
    assert!(s.execute("sphere360.patchPole", json!({"donor_document":donor,"horizontal_fov":90})).is_err());
    assert!(Arc::ptr_eq(&before, &s.active().unwrap().doc));
    s.set_inline_job_ctx(None);
    s.edit("lock alpha", |doc, active| {
        doc.layer_mut(active.unwrap()).unwrap().locks.transparency = true;
        Ok(())
    })
    .unwrap();
    assert!(!s.is_enabled("sphere360.patchPole"));
    assert!(s.is_enabled("sphere360.denoise"));
}

#[test]
fn denoise_shared_colour_io_retains_alpha_and_exact_noop_undo() {
    for depth in SampleType::ALL {
        let (mut s, _) = fixture(depth);
        s.edit("noise", |doc, active| {
            let surface = doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap();
            for y in 0..32 {
                for x in 1..64 {
                    let v = 0.4 + ((x * 17 + y * 31) % 19) as f32 * 0.005;
                    surface.write_pixel(x, y, &[v, v, v, 0.5]);
                }
            }
            Ok(())
        })
        .unwrap();
        let before = bytes(&s, 0);
        s.execute("sphere360.denoise", json!({"strength":0})).unwrap();
        assert_eq!(bytes(&s, 0), before);
        s.execute("sphere360.denoise", json!({"sigma":0.03})).unwrap();
        assert_ne!(bytes(&s, 0), before);
        let state = s.active().unwrap();
        let surface = state.doc.layer(state.active_layer.unwrap()).unwrap().surface().unwrap();
        assert_eq!(pixel(surface, 0, 0)[3], 0.0);
        assert!((pixel(surface, 1, 0)[3] - 0.5).abs() < 0.005);
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(bytes(&s, 0), before);
    }
}
