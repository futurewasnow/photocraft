//! Full-sphere operations with explicit scope. Yaw shifts native channels without colour conversion
//! or resampling, so integer, float, RGB, CMYK and Lab pixels retain their values exactly.
use crate::{EngineError, Result, Session, commands::CommandSpec};
use photocraft_doc::LayerContent;
use serde_json::{Value, json};

const MAX_BAND_SAMPLES: usize = 2 * 1024 * 1024;

fn band_rows(width: u32, channels: usize) -> Result<usize> {
    let samples = (width as usize).checked_mul(channels).filter(|n| *n > 0 && *n <= MAX_BAND_SAMPLES);
    match samples {
        Some(n) => Ok((MAX_BAND_SAMPLES / n).min(128)),
        None => Err(EngineError::Other("360 yaw row exceeds the 8 MiB working-buffer limit".into())),
    }
}

fn enabled(s: &Session) -> std::result::Result<(), String> {
    let state = s.active().ok_or("no document open")?;
    let size = state.doc.size;
    if size.height == 0 || u64::from(size.width) != 2 * u64::from(size.height) {
        return Err("360 yaw requires a full-sphere 2:1 canvas".into());
    }
    if state.doc.selection.is_some() {
        return Err("deselect before shifting the complete sphere".into());
    }
    if state.channel_view.target != crate::channel_cmds::ChannelTarget::Composite || state.doc.quick_mask.is_some() {
        return Err("select composite layer pixels and leave Quick Mask before shifting the sphere".into());
    }
    let id = state.active_layer.ok_or("no active layer")?;
    let layer = state.doc.layer(id).ok_or("active layer missing")?;
    if layer.mask.is_some() || layer.vector_mask.is_some() {
        return Err("360 yaw requires an unmasked pixel layer; apply or remove masks first".into());
    }
    if !matches!(layer.content, LayerContent::Raster(_)) {
        return Err("360 yaw requires a raster layer".into());
    }
    crate::commands::check_pixels_unlocked(&state.doc, id).map_err(|e| e.to_string())?;
    if state.doc.effective_locks(id).position {
        return Err("unlock the layer position before moving sphere pixels".into());
    }
    if state.doc.effective_locks(id).transparency {
        return Err("unlock transparency before moving sphere pixels".into());
    }
    Ok(())
}
fn yaw(s: &mut Session, p: &Value) -> Result<Value> {
    enabled(s).map_err(EngineError::Other)?;
    let object = p.as_object().ok_or_else(|| EngineError::Other("parameters must be an object".into()))?;
    if object.keys().any(|k| k != "columns") {
        return Err(EngineError::Other("only the columns parameter is supported".into()));
    }
    let columns = p.get("columns").and_then(Value::as_i64).ok_or_else(|| EngineError::Other("columns must be an integer".into()))?;
    crate::jobs::edit_job(
        s,
        "360 Lossless Yaw",
        move |doc, active, ctx| {
            ctx.check()?;
            let id = active.ok_or_else(|| EngineError::Other("no active layer".into()))?;
            let (w, h) = (doc.size.width, doc.size.height);
            if w > i32::MAX as u32 || h > i32::MAX as u32 {
                return Err(EngineError::Other("360 yaw canvas exceeds supported coordinates".into()));
            }
            let layer = doc.layer_mut(id).ok_or(EngineError::NoLayer(id))?;
            let surface = layer.surface_mut().ok_or_else(|| EngineError::Other("not a raster layer".into()))?;
            let channels = surface.channels();
            let rows = band_rows(w, channels)?;
            let shift = columns.rem_euclid(i64::from(w)) as usize * channels;
            // Each band's source and destination use the same rows, so later reads cannot
            // observe a previously shifted row. Only the staged document is modified.
            for y0 in (0..h).step_by(rows) {
                ctx.check()?;
                let y1 = y0.saturating_add(rows as u32).min(h);
                let bounds = photocraft_geom::Rect::new(0, y0 as i32, w as i32, y1 as i32);
                let mut data = surface.try_read_region(bounds).map_err(|e| EngineError::Other(e.to_string()))?;
                for row in data.chunks_exact_mut(w as usize * channels) {
                    row.rotate_right(shift);
                }
                ctx.check()?;
                surface.try_write_region(bounds, &data).map_err(|e| EngineError::Other(e.to_string()))?;
                ctx.progress(y1 as f32 / h as f32, "Shifting panorama heading");
            }
            ctx.check()?;
            Ok(
                json!({"layer":id.0,"columns":columns,"degrees":columns.rem_euclid(i64::from(w)) as f64*360.0/f64::from(w),"scope":"active raster layer","resampled":false}),
            )
        },
        |result| result,
    )
}
pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "sphere360.yaw",
        label: "360 Lossless Yaw",
        menu: &[],
        shortcut: None,
        params: r#"{"columns":integer (positive moves pixels right; full 2:1 raster layer; no selection)}"#,
        enabled,
        run: yaw,
        journal: true,
    }]
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_models_alpha_and_hdr_samples_are_retained() {
        use photocraft_color::{ColorMode, PixelFormat, SampleType};
        for mode in [ColorMode::Rgb, ColorMode::Grayscale, ColorMode::Cmyk, ColorMode::Lab] {
            for depth in SampleType::ALL {
                for alpha in [false, true] {
                    let format = PixelFormat::new(mode, depth, alpha);
                    let mut doc = photocraft_doc::Document::new("native model", photocraft_geom::Size::new(16, 8), mode, depth);
                    let mut layer = photocraft_doc::Layer::raster("panorama", format);
                    let surface = layer.surface_mut().unwrap();
                    for x in 0..16 {
                        let values: Vec<_> = (0..format.channels()).map(|c| (x as f32 + c as f32) / 10. - 0.25).collect();
                        surface.write_pixel(x, 0, &values);
                    }
                    let original = surface.try_to_interleaved(doc.bounds()).unwrap();
                    let id = layer.id;
                    doc.layers.push(layer);
                    let mut s = Session::new();
                    s.add_document(doc, None);
                    s.execute("sphere360.yaw", json!({"columns":5})).unwrap();
                    let state = s.active().unwrap();
                    let shifted = state.doc.layer(id).unwrap().surface().unwrap().try_to_interleaved(state.doc.bounds()).unwrap();
                    let mut expected = original.clone();
                    for row in expected.chunks_exact_mut(16 * format.bytes_per_pixel()) {
                        row.rotate_right(5 * format.bytes_per_pixel());
                    }
                    assert_eq!(shifted, expected, "{mode:?} {depth:?} alpha={alpha}");
                    s.execute("sphere360.yaw", json!({"columns":-5})).unwrap();
                    let state = s.active().unwrap();
                    assert_eq!(state.doc.layer(id).unwrap().surface().unwrap().try_to_interleaved(state.doc.bounds()).unwrap(), original);
                }
            }
        }
    }
    #[test]
    fn band_shift_preserves_rows_and_has_bounded_scratch() {
        for width in [260, 8192, 12000, 300000] {
            let rows = band_rows(width, 4).unwrap();
            assert!(rows > 0 && rows <= 128);
            assert!(rows * width as usize * 4 <= MAX_BAND_SAMPLES);
        }
        assert!(band_rows(0, 4).is_err());
        assert!(band_rows(16, 0).is_err());
        assert!(band_rows(u32::MAX, usize::MAX).is_err());
        let mut s = Session::new();
        s.execute("file.new", json!({"width":260,"height":130,"depth":32,"background":"transparent"})).unwrap();
        s.edit("fixture", |doc, active| {
            let surface = doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap();
            for y in 0..130 {
                surface.write_pixel(259, y, &[y as f32 / 130., 0.25, 0.75, 1.]);
            }
            Ok(())
        })
        .unwrap();
        let original = s.active().unwrap().doc.clone();
        s.execute("sphere360.yaw", json!({"columns":1})).unwrap();
        let current = s.active().unwrap();
        let id = current.active_layer.unwrap();
        let expected = original.layer(id).unwrap().surface().unwrap().read_region(photocraft_geom::Rect::new(259, 0, 260, 130));
        let actual = current.doc.layer(id).unwrap().surface().unwrap().read_region(photocraft_geom::Rect::new(0, 0, 1, 130));
        assert_eq!(actual, expected);
        s.execute("sphere360.yaw", json!({"columns":-1})).unwrap();
        assert_eq!(
            s.active().unwrap().doc.layer(id).unwrap().surface().unwrap().read_region(original.bounds()),
            original.layer(id).unwrap().surface().unwrap().read_region(original.bounds())
        );
    }
    #[test]
    fn native_depths_cancel_masks_and_redo() {
        for depth in [8, 16, 32] {
            let mut s = Session::new();
            s.execute("file.new", json!({"width":16,"height":8,"depth":depth,"background":"transparent"})).unwrap();
            s.edit("fixture", |doc, active| {
                let surface = doc.layer_mut(active.unwrap()).unwrap().surface_mut().unwrap();
                surface.write_pixel(0, 0, &[0.12345, 0.25, 0.75, 1.]);
                Ok(())
            })
            .unwrap();
            let original = s.active().unwrap().doc.clone();
            let ctx = crate::jobs::JobCtx::new();
            ctx.cancel();
            s.set_inline_job_ctx(Some(ctx));
            assert!(s.execute("sphere360.yaw", json!({"columns":8})).is_err());
            assert!(std::sync::Arc::ptr_eq(&original, &s.active().unwrap().doc));
            s.set_inline_job_ctx(None);
            s.execute("sphere360.yaw", json!({"columns":8})).unwrap();
            s.execute("edit.undo", json!({})).unwrap();
            s.execute("edit.redo", json!({})).unwrap();
            let current = s.active().unwrap();
            let id = current.active_layer.unwrap();
            let before = original.layer(id).unwrap().surface().unwrap().read_region(photocraft_geom::Rect::new(0, 0, 1, 1));
            let after = current.doc.layer(id).unwrap().surface().unwrap().read_region(photocraft_geom::Rect::new(8, 0, 9, 1));
            assert_eq!(before, after);
            let saved = photocraft_format::save_to_bytes(&current.doc, &Default::default()).unwrap();
            let reopened = photocraft_format::load_from_bytes(&saved).unwrap();
            let restored = reopened.layers[0].surface().unwrap();
            assert_eq!(restored.format(), current.doc.layer(id).unwrap().surface().unwrap().format());
            assert_eq!(restored.read_region(reopened.bounds()), current.doc.layer(id).unwrap().surface().unwrap().read_region(current.doc.bounds()));
            s.edit("position lock", |doc, active| {
                doc.layer_mut(active.unwrap()).unwrap().locks.position = true;
                Ok(())
            })
            .unwrap();
            assert!(!s.is_enabled("sphere360.yaw"));
            assert!(s.execute("sphere360.yaw", json!({"columns":8})).is_err());
            s.execute("edit.undo", json!({})).unwrap();
            use crate::channel_cmds::ChannelTarget;
            for target in [ChannelTarget::Color(0), ChannelTarget::Color(1), ChannelTarget::Color(2)] {
                s.active_mut().unwrap().channel_view.target = target;
                assert!(!s.is_enabled("sphere360.yaw"));
                assert!(s.execute("sphere360.yaw", json!({"columns":8})).is_err());
            }
            s.active_mut().unwrap().channel_view.target = ChannelTarget::Composite;
            s.execute("select.editInQuickMaskMode", json!({"on":true})).unwrap();
            assert!(!s.is_enabled("sphere360.yaw"));
            assert!(s.execute("sphere360.yaw", json!({"columns":8})).is_err());
            s.execute("select.editInQuickMaskMode", json!({"on":false})).unwrap();
            s.edit("mask", |doc, active| {
                doc.layer_mut(active.unwrap()).unwrap().mask = Some(photocraft_doc::LayerMask::reveal_all());
                Ok(())
            })
            .unwrap();
            assert!(!s.is_enabled("sphere360.yaw"));
            assert!(s.execute("sphere360.yaw", json!({"columns":8})).is_err());
        }
    }
    #[test]
    fn yaw_roundtrip_undo_and_bad_input() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width":16,"height":8})).unwrap();
        s.execute("layer.new.layer", json!({})).unwrap();
        s.execute("edit.fill", json!({"color":"#3366cc"})).unwrap();
        s.edit("fixture", |doc, active| {
            let id = active.ok_or_else(|| EngineError::Other("missing layer".into()))?;
            let surface = doc.layer_mut(id).unwrap().surface_mut().unwrap();
            let n = surface.channels();
            surface.write_pixel(0, 0, &vec![1.0; n]);
            surface.write_pixel(15, 0, &vec![0.0; n]);
            Ok(())
        })
        .unwrap();
        let before = s.active().unwrap().doc.clone();
        assert!(s.execute("sphere360.yaw", json!({"columns":"oops"})).is_err());
        s.execute("sphere360.yaw", json!({"columns":3})).unwrap();
        {
            let state = s.active().unwrap();
            let id = state.active_layer.unwrap();
            let surface = state.doc.layer(id).unwrap().surface().unwrap();
            let row = surface.read_region(photocraft_geom::Rect::new(0, 0, 16, 1));
            let original = before.layer(id).unwrap().surface().unwrap().read_region(photocraft_geom::Rect::new(0, 0, 16, 1));
            let n = surface.channels();
            assert_eq!(&row[3 * n..4 * n], &original[..n]);
            assert_eq!(&row[2 * n..3 * n], &original[15 * n..16 * n]);
        }
        s.execute("edit.undo", json!({})).unwrap();
        let state = s.active().unwrap();
        let id = state.active_layer.unwrap();
        assert_eq!(
            state.doc.layer(id).unwrap().surface().unwrap().read_region(state.doc.bounds()),
            before.layer(id).unwrap().surface().unwrap().read_region(before.bounds())
        );
        s.execute("sphere360.yaw", json!({"columns":i64::MIN})).unwrap();
    }
    #[test]
    fn rejects_flat_canvas_and_empty_session() {
        let mut s = Session::new();
        assert!(s.execute("sphere360.yaw", json!({"columns":1})).is_err());
        s.execute("file.new", json!({"width":8,"height":8})).unwrap();
        assert!(s.execute("sphere360.yaw", json!({"columns":1})).is_err());
    }
}
