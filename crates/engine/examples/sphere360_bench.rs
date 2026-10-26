//! Manual release benchmark and independent full-buffer oracle for the banded heading edit.
//! `cargo run --release -p photocraft-engine --example sphere360_bench`
use photocraft_engine::Session;
use photocraft_geom::Rect;
use serde_json::json;
use std::time::Instant;

fn digest(surface: &photocraft_raster::Surface, width: u32, height: u32) -> Result<String, Box<dyn std::error::Error>> {
    let mut hash = blake3::Hasher::new();
    for y in (0..height).step_by(64) {
        let bytes = surface.try_to_interleaved(Rect::new(0, y as i32, width as i32, y.saturating_add(64).min(height) as i32))?;
        hash.update(&bytes);
    }
    Ok(hash.finalize().to_hex().to_string())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if cfg!(debug_assertions) {
        return Err("run this benchmark with --release".into());
    }
    let (w, h) = (8192_u32, 4096_u32);
    let columns = 3073;
    let mut fixture = Session::new();
    fixture.execute("file.new", json!({"width":w,"height":h,"depth":16,"background":"transparent"}))?;
    fixture.edit("procedural benchmark fixture", |doc, active| {
        let id = active.ok_or_else(|| photocraft_engine::EngineError::Other("fixture layer missing".into()))?;
        let surface = doc.layer_mut(id).and_then(|l| l.surface_mut()).ok_or_else(|| photocraft_engine::EngineError::Other("fixture surface missing".into()))?;
        for y0 in (0..h).step_by(64) {
            let rows = 64.min(h - y0);
            let mut data = vec![0.; w as usize * rows as usize * 4];
            for (i, px) in data.chunks_exact_mut(4).enumerate() {
                let x = i as u32 % w;
                let y = y0 + i as u32 / w;
                px.copy_from_slice(&[x as f32 / w as f32, y as f32 / h as f32, ((x * 13 + y * 17) % 4093) as f32 / 4093., 1.]);
            }
            surface
                .try_write_region(Rect::new(0, y0 as i32, w as i32, (y0 + rows) as i32), &data)
                .map_err(|e| photocraft_engine::EngineError::Other(e.to_string()))?;
        }
        Ok(())
    })?;
    let source = fixture.active().ok_or("missing fixture document")?.doc.clone();
    let id = fixture.active().and_then(|d| d.active_layer).ok_or("missing fixture layer")?;
    let bounds = source.bounds();
    let mut measurements = Vec::new();
    for iteration in 0..3 {
        let mut reference = Session::new();
        reference.add_document((*source).clone(), None);
        let start = Instant::now();
        reference.edit("full-buffer reference", |doc, _| {
            let surface =
                doc.layer_mut(id).and_then(|l| l.surface_mut()).ok_or_else(|| photocraft_engine::EngineError::Other("reference surface missing".into()))?;
            let channels = surface.channels();
            let mut data = surface.try_read_region(bounds).map_err(|e| photocraft_engine::EngineError::Other(e.to_string()))?;
            for row in data.chunks_exact_mut(w as usize * channels) {
                row.rotate_right(columns * channels);
            }
            surface.try_write_region(bounds, &data).map_err(|e| photocraft_engine::EngineError::Other(e.to_string()))?;
            Ok(())
        })?;
        let reference_seconds = start.elapsed().as_secs_f64();
        let reference_doc = &reference.active().ok_or("missing reference document")?.doc;
        let expected = digest(reference_doc.layer(id).and_then(|l| l.surface()).ok_or("missing reference surface")?, w, h)?;
        drop(reference);
        let mut banded = Session::new();
        banded.add_document((*source).clone(), None);
        let start = Instant::now();
        banded.execute("sphere360.yaw", json!({"columns":columns}))?;
        let banded_seconds = start.elapsed().as_secs_f64();
        let actual_doc = &banded.active().ok_or("missing shifted document")?.doc;
        let actual = digest(actual_doc.layer(id).and_then(|l| l.surface()).ok_or("missing shifted surface")?, w, h)?;
        if actual != expected {
            return Err("banded output differs from the full-buffer reference".into());
        }
        measurements.push(json!({"iteration":iteration,"reference_seconds":reference_seconds,"banded_seconds":banded_seconds,"blake3":actual}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"width":w,"height":h,"depth":16,"columns":columns,"reference_scratch_bytes":u64::from(w)*u64::from(h)*16,"band_scratch_limit_bytes":8*1024*1024,"measurements":measurements})
        )?
    );
    Ok(())
}
