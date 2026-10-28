//! Manual 33.6 MP release benchmark of colour-managed denoise, or pole repair with `--pole`.
use photocraft_engine::{EngineError, Session};
use photocraft_geom::Rect;
use serde_json::json;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if cfg!(debug_assertions) {
        return Err("run this benchmark with --release".into());
    }
    let (width, height) = (8192u32, 4096u32);
    let mut session = Session::new();
    session.execute("file.new", json!({"width":width,"height":height,"depth":16,"background":"transparent"}))?;
    session.edit("procedural noisy sphere", |doc, active| {
        let id = active.ok_or_else(|| EngineError::Other("missing layer".into()))?;
        let surface = doc.layer_mut(id).and_then(|l| l.surface_mut()).ok_or_else(|| EngineError::Other("missing surface".into()))?;
        let mut random = 123456789u32;
        for y in (0..height).step_by(64) {
            let rows = 64.min(height - y);
            let mut data = vec![0.; width as usize * rows as usize * 4];
            for pixel in data.as_chunks_mut::<4>().0 {
                random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                let value = 0.45 + ((random >> 8) as f32 / 16777216.0 - 0.5) * 0.12;
                *pixel = [value, value, value, 1.0];
            }
            surface.try_write_region(Rect::new(0, y as i32, width as i32, (y + rows) as i32), &data).map_err(|e| EngineError::Other(e.to_string()))?;
        }
        Ok(())
    })?;
    let original = session.active().ok_or("missing document")?.doc.clone();
    let pole = std::env::args().any(|arg| arg == "--pole");
    let donor = if pole {
        session.execute("file.new", json!({"width":512,"height":512,"depth":16,"background":"white"}))?;
        let id = session.active().ok_or("missing donor document")?.doc.id.0;
        if !session.set_active(0) {
            return Err("missing target document".into());
        }
        Some(id)
    } else {
        None
    };
    let started = Instant::now();
    let report = if let Some(donor) = donor {
        session.execute("sphere360.patchPole", json!({"donor_document":donor,"horizontal_fov":90}))?
    } else {
        session.execute("sphere360.denoise", json!({}))?
    };
    let seconds = started.elapsed().as_secs_f64();
    if !pole && report["noise"]["applied"] != true {
        return Err("benchmark did not exercise denoising".into());
    }
    let finished = session.active().ok_or("missing output")?.doc.clone();
    let id = session.active().and_then(|s| s.active_layer).ok_or("missing output layer")?;
    session.execute("edit.undo", json!({}))?;
    let restored = session.active().ok_or("missing restored document")?.doc.clone();
    let mut source_hash = blake3::Hasher::new();
    let mut output_hash = blake3::Hasher::new();
    for y in (0..height).step_by(64) {
        let bounds = Rect::new(0, y as i32, width as i32, y.saturating_add(64).min(height) as i32);
        let read = |doc: &photocraft_doc::Document| -> Result<Vec<u8>, Box<dyn std::error::Error>> {
            Ok(doc.layer(id).and_then(|l| l.surface()).ok_or("missing surface")?.try_to_interleaved(bounds)?)
        };
        let source = read(&original)?;
        if source != read(&restored)? {
            return Err("undo failed to restore source bytes".into());
        }
        source_hash.update(&source);
        output_hash.update(&read(&finished)?);
    }
    if source_hash.finalize() == output_hash.finalize() {
        return Err("benchmark output did not change".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"command":if pole {"sphere360.patchPole"} else {"sphere360.denoise"},"width":width,"height":height,"depth":16,"seconds":seconds,"report":report,"undo_bit_exact":true,"source_blake3":source_hash.finalize().to_hex().to_string(),"output_blake3":output_hash.finalize().to_hex().to_string(),"linear_frame_storage_bytes":if pole {0} else {u64::from(width)*u64::from(height)*32},"linear_band_storage_bytes":if pole {report["linear_band_storage_bytes"].as_u64().unwrap_or(0)} else {0},"latitude_tap_storage_bytes":if pole {0} else {u64::from(height)*576},"donor_linear_bytes":if pole {512*512*16} else {0},"native_band_limit_bytes":8*1024*1024,"note":"single-host synthetic measurement, no baseline or quality approval"})
        )?
    );
    Ok(())
}
