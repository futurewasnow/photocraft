//! Colour-managed full-sphere finishing. Work is staged by edit_job and committed once.
use crate::{EngineError, Result, Session, commands::CommandSpec, jobs::JobCtx};
use photocraft_algo::sphere360::{
    self as sphere,
    finish::{DenoiseOptions, DenoiseReport},
};
use photocraft_cms::{Builtin, Intent, Transform};
use photocraft_color::ColorMode;
use photocraft_geom::Rect;
use serde_json::{Value, json};

fn failure(error: impl std::fmt::Display) -> EngineError {
    EngineError::Other(error.to_string())
}
fn sphere_error(error: sphere::Error) -> EngineError {
    match error {
        sphere::Error::Cancelled => EngineError::Cancelled,
        other => failure(other),
    }
}
fn enabled(s: &Session) -> std::result::Result<(), String> {
    let state = s.active().ok_or("no document open")?;
    let doc = &state.doc;
    if doc.size.height == 0 || u64::from(doc.size.width) != 2 * u64::from(doc.size.height) {
        return Err("360 denoise requires a monoscopic full-sphere 2:1 canvas".into());
    }
    if !matches!(doc.mode, ColorMode::Rgb | ColorMode::Grayscale) {
        return Err("360 denoise currently supports RGB and grayscale; convert a copy through Image > Mode first".into());
    }
    if doc.selection.is_some() || doc.quick_mask.is_some() || state.channel_view.target != crate::channel_cmds::ChannelTarget::Composite {
        return Err("deselect, leave Quick Mask and target composite layer pixels before 360 denoising".into());
    }
    let id = state.active_layer.ok_or("no active layer")?;
    let layer = doc.layer(id).ok_or("active layer missing")?;
    let surface = layer.surface().ok_or("360 denoise requires a raster layer")?;
    if surface.format().mode != doc.mode {
        return Err("layer colour mode differs from the document".into());
    }
    if layer.mask.is_some() || layer.vector_mask.is_some() {
        return Err("apply or remove layer masks before full-sphere denoising".into());
    }
    crate::commands::check_pixels_unlocked(doc, id).map_err(|e| e.to_string())?;
    sphere::pixel_count(doc.size.width, doc.size.height).map_err(|e| e.to_string())?;
    Ok(())
}

/// Bands bound native-channel scratch; the two linear RGBA frames are bounded by sphere-core.
fn process(
    surface: &mut photocraft_raster::Surface,
    width: u32,
    height: u32,
    to_linear: &Transform,
    from_linear: &Transform,
    options: DenoiseOptions,
    ctx: &JobCtx,
) -> Result<DenoiseReport> {
    ctx.check()?;
    let format = surface.format();
    let colors = format.mode.color_channels();
    let channels = format.channels();
    if width > i32::MAX as u32
        || height > i32::MAX as u32
        || to_linear.inputs() != colors
        || to_linear.outputs() != 3
        || from_linear.inputs() != 3
        || from_linear.outputs() != colors
    {
        return Err(failure("unsupported spherical colour transform or canvas dimensions"));
    }
    let row_samples = (width as usize)
        .checked_mul(channels)
        .filter(|n| *n > 0 && *n <= 2 * 1024 * 1024)
        .ok_or_else(|| failure("360 denoise row exceeds the 8 MiB band limit"))?;
    let rows = (2 * 1024 * 1024 / row_samples).min(128);
    let mut input = sphere::pixels(width, height).map_err(sphere_error)?;
    for y in (0..height).step_by(rows) {
        ctx.check()?;
        let end = y.saturating_add(rows as u32).min(height);
        let data = surface.try_read_region(Rect::new(0, y as i32, width as i32, end as i32)).map_err(failure)?;
        let offset = y as usize * width as usize;
        let count = (end - y) as usize * width as usize;
        let destination = input.get_mut(offset..offset + count).ok_or_else(|| failure("invalid 360 conversion band"))?;
        for (native, rgba) in data.chunks_exact(channels).zip(destination) {
            if native.iter().any(|v| !v.is_finite()) {
                return Err(failure("360 denoise requires finite source pixels"));
            }
            let alpha = if format.alpha { native.get(colors).copied().ok_or_else(|| failure("missing alpha channel"))? } else { 1.0 };
            if !(0.0..=1.0).contains(&alpha) {
                return Err(failure("360 denoise requires alpha in 0..1"));
            }
            let mut linear = [0.; 3];
            to_linear.eval(native.get(..colors).ok_or_else(|| failure("missing colour channels"))?, &mut linear);
            *rgba = [linear[0] * alpha, linear[1] * alpha, linear[2] * alpha, alpha];
        }
        ctx.progress(0.15 * end as f32 / height as f32, "Preparing linear 360 pixels");
    }
    let panorama = sphere::Panorama::new(width, height, &input).map_err(sphere_error)?;
    let finished = sphere::finish::denoise_with_control(&panorama, options, &|progress| {
        ctx.progress(0.15 + progress * 0.7, "Denoising the sphere");
        !ctx.cancelled()
    })
    .map_err(sphere_error)?;
    // A no-op must be bit-exact, including hidden RGB and non-linear profile round trips.
    if !finished.report.applied {
        ctx.check()?;
        return Ok(finished.report);
    }
    for y in (0..height).step_by(rows) {
        ctx.check()?;
        let end = y.saturating_add(rows as u32).min(height);
        let bounds = Rect::new(0, y as i32, width as i32, end as i32);
        let mut native = surface.try_read_region(bounds).map_err(failure)?;
        let offset = y as usize * width as usize;
        let count = (end - y) as usize * width as usize;
        let pixels = finished.pixels.get(offset..offset + count).ok_or_else(|| failure("invalid 360 output band"))?;
        let originals = input.get(offset..offset + count).ok_or_else(|| failure("invalid 360 input band"))?;
        for ((pixel, original), destination) in pixels.iter().zip(originals).zip(native.chunks_exact_mut(channels)) {
            // Retain alpha and hidden native samples exactly. Also skip unchanged pixels.
            if pixel == original || pixel[3] < 1e-6 {
                continue;
            }
            let straight = [pixel[0] / pixel[3], pixel[1] / pixel[3], pixel[2] / pixel[3]];
            let output = destination.get_mut(..colors).ok_or_else(|| failure("invalid native colour channels"))?;
            from_linear.eval(&straight, output);
            if output.iter().any(|v| !v.is_finite()) {
                return Err(failure("360 denoise colour conversion produced non-finite pixels"));
            }
        }
        ctx.check()?;
        surface.try_write_region(bounds, &native).map_err(failure)?;
        ctx.progress(0.85 + 0.15 * end as f32 / height as f32, "Saving denoised layer pixels");
    }
    ctx.check()?;
    Ok(finished.report)
}

fn denoise(s: &mut Session, params: &Value) -> Result<Value> {
    enabled(s).map_err(failure)?;
    if !params.is_object() {
        return Err(failure("denoise parameters must be an object"));
    }
    let options: DenoiseOptions = serde_json::from_value(params.clone()).map_err(failure)?;
    let doc = &s.active().ok_or_else(|| failure("no document"))?.doc;
    let profile = match &doc.icc_profile {
        Some(bytes) => crate::color_cmds::profile_from_bytes(bytes).map_err(failure)?,
        None => s.color.working(doc.mode),
    };
    if profile.color_space != crate::color_cmds::mode_space(doc.mode) {
        return Err(failure("embedded profile does not match the document colour mode"));
    }
    let linear = Builtin::LinearSrgb.profile();
    let to_linear = Transform::new(&profile, linear, Intent::RelativeColorimetric, false).map_err(failure)?;
    let from_linear = Transform::new(linear, &profile, Intent::RelativeColorimetric, false).map_err(failure)?;
    crate::jobs::edit_job(
        s,
        "360 Denoise",
        move |doc, active, ctx| {
            let (width, height) = (doc.size.width, doc.size.height);
            let id = active.ok_or_else(|| failure("no active layer"))?;
            let surface = doc.layer_mut(id).ok_or(EngineError::NoLayer(id))?.surface_mut().ok_or_else(|| failure("not a raster layer"))?;
            let report = process(surface, width, height, &to_linear, &from_linear, options, ctx)?;
            Ok(
                json!({"layer":id.0,"scope":"active raster layer","width":width,"height":height,"noise":report,"method":"spherical spatial bilateral; not temporal video denoising"}),
            )
        },
        |value| value,
    )
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "sphere360.denoise",
        label: "360 Denoise",
        menu: &[],
        shortcut: None,
        params: r#"{"strength":0..1 (default .8),"radius_pixels":.5..3 (default 1.5),"sigma":null|0..0.15 (linear RGB; null estimates noise)}"#,
        enabled,
        run: denoise,
        journal: true,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(depth: u32) -> Session {
        let mut s = Session::new();
        s.execute("file.new", json!({"width":64,"height":32,"depth":depth,"background":"transparent"})).unwrap();
        s.edit("noise", |doc, active| {
            let surface = doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap();
            for y in 0..32 {
                for x in 0..64 {
                    let v = 0.45 + (((x * 17 + y * 31) % 19) as f32 - 9.) * 0.006;
                    surface.write_pixel(
                        x,
                        y,
                        &[
                            v,
                            v,
                            v,
                            if x == 0 {
                                0.
                            } else if x == 1 {
                                0.5
                            } else {
                                1.
                            },
                        ],
                    );
                }
            }
            Ok(())
        })
        .unwrap();
        s
    }
    fn bytes(s: &Session) -> Vec<u8> {
        let state = s.active().unwrap();
        state.doc.layer(state.active_layer.unwrap()).unwrap().surface().unwrap().try_to_interleaved(state.doc.bounds()).unwrap()
    }
    #[test]
    fn depths_alpha_hidden_pixels_undo_redo_and_noop() {
        for depth in [8, 16, 32] {
            let mut s = fixture(depth);
            let original = bytes(&s);
            s.execute("sphere360.denoise", json!({"strength":0})).unwrap();
            assert_eq!(bytes(&s), original);
            let before = s.active().unwrap().doc.clone();
            let result = s.execute("sphere360.denoise", json!({"sigma":0.03})).unwrap();
            assert_eq!(result["noise"]["applied"], true);
            let finished = bytes(&s);
            assert_ne!(finished, original);
            let state = s.active().unwrap();
            let id = state.active_layer.unwrap();
            let a = before.layer(id).unwrap().surface().unwrap().read_region(before.bounds());
            let b = state.doc.layer(id).unwrap().surface().unwrap().read_region(state.doc.bounds());
            for (i, (a, b)) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0.iter()).enumerate() {
                assert_eq!(a[3], b[3]);
                if i % 64 == 0 {
                    assert_eq!(a, b);
                }
            }
            s.execute("edit.undo", json!({})).unwrap();
            assert_eq!(bytes(&s), original);
            s.execute("edit.redo", json!({})).unwrap();
            assert_eq!(bytes(&s), finished);
        }
    }
    #[test]
    fn invalid_parameters_profile_and_cancel_leave_document_unchanged() {
        let mut s = fixture(32);
        for params in [json!(null), json!({"unknown":true}), json!({"strength":-1}), json!({"sigma":9}), json!({"radius_pixels":1e100})] {
            let before = s.active().unwrap().doc.clone();
            assert!(s.execute("sphere360.denoise", params).is_err());
            assert!(std::sync::Arc::ptr_eq(&before, &s.active().unwrap().doc));
        }
        let ctx = JobCtx::new();
        ctx.cancel();
        s.set_inline_job_ctx(Some(ctx));
        let before = s.active().unwrap().doc.clone();
        assert!(s.execute("sphere360.denoise", json!({})).is_err());
        assert!(std::sync::Arc::ptr_eq(&before, &s.active().unwrap().doc));
        s.set_inline_job_ctx(None);
        s.edit("broken profile", |doc, _| {
            doc.icc_profile = Some(std::sync::Arc::new(vec![0; 128]));
            Ok(())
        })
        .unwrap();
        let before = s.active().unwrap().doc.clone();
        assert!(s.execute("sphere360.denoise", json!({})).is_err());
        assert!(std::sync::Arc::ptr_eq(&before, &s.active().unwrap().doc));
    }

    #[test]
    fn embedded_linear_profile_matches_kernel_instead_of_assuming_srgb() {
        let mut s = fixture(32);
        s.edit("linear working pixels", |doc, active| {
            doc.icc_profile = Some(Builtin::LinearSrgb.profile().to_bytes());
            let surface = doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap();
            for y in 0..8 {
                for x in 16..24 {
                    surface.write_pixel(x, y, &[1.5 + (x % 3) as f32 * 0.01, -0.02, 0.8, 1.0]);
                }
            }
            Ok(())
        })
        .unwrap();
        let state = s.active().unwrap();
        let pixels = state.doc.layer(state.active_layer.unwrap()).unwrap().surface().unwrap().read_region(state.doc.bounds());
        let input: Vec<_> = pixels.as_chunks::<4>().0.iter().map(|p| [p[0] * p[3], p[1] * p[3], p[2] * p[3], p[3]]).collect();
        let expected =
            sphere::finish::denoise(&sphere::Panorama::new(64, 32, &input).unwrap(), DenoiseOptions { sigma: Some(0.03), ..Default::default() }).unwrap();
        s.execute("sphere360.denoise", json!({"sigma":0.03})).unwrap();
        let state = s.active().unwrap();
        let output = state.doc.layer(state.active_layer.unwrap()).unwrap().surface().unwrap().read_region(state.doc.bounds());
        for (actual, expected) in output.as_chunks::<4>().0.iter().zip(expected.pixels) {
            if expected[3] > 0.0 {
                for c in 0..3 {
                    assert!((actual[c] - expected[c] / expected[3]).abs() < 0.0001);
                }
            }
        }
        assert_eq!(state.doc.icc_profile, Some(Builtin::LinearSrgb.profile().to_bytes()));
    }

    #[test]
    fn grayscale_depths_and_edit_scope_are_explicit() {
        use photocraft_color::{PixelFormat, SampleType};
        for depth in SampleType::ALL {
            let mut doc = photocraft_doc::Document::new("gray", photocraft_geom::Size::new(32, 16), ColorMode::Grayscale, depth);
            let mut layer = photocraft_doc::Layer::raster("sphere", PixelFormat::new(ColorMode::Grayscale, depth, true));
            for y in 0..16 {
                for x in 0..32 {
                    layer.surface_mut().unwrap().write_pixel(x, y, &[0.5 + (x % 3) as f32 * 0.03, 0.75]);
                }
            }
            let id = layer.id;
            doc.layers.push(layer);
            let mut s = Session::new();
            s.add_document(doc, None);
            s.execute("sphere360.denoise", json!({"sigma":0.03})).unwrap();
            let state = s.active().unwrap();
            let surface = state.doc.layer(id).unwrap().surface().unwrap();
            assert_eq!(surface.format(), PixelFormat::new(ColorMode::Grayscale, depth, true));
            s.edit("mask", |doc, _| {
                doc.layer_mut(id).unwrap().mask = Some(photocraft_doc::LayerMask::reveal_all());
                Ok(())
            })
            .unwrap();
            assert!(!s.is_enabled("sphere360.denoise"));
        }
        let mut s = fixture(16);
        s.active_mut().unwrap().channel_view.target = crate::channel_cmds::ChannelTarget::Color(0);
        assert!(!s.is_enabled("sphere360.denoise"));
        s.active_mut().unwrap().channel_view.target = crate::channel_cmds::ChannelTarget::Composite;
        s.edit("pixel lock", |doc, active| {
            doc.layer_mut(active.unwrap()).unwrap().locks.pixels = true;
            Ok(())
        })
        .unwrap();
        assert!(!s.is_enabled("sphere360.denoise"));
    }
}
