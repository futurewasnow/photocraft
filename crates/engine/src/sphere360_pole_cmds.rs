//! Pole-cap repairs from another open document's active raster pixels.
use crate::sphere360_finish_cmds::{LinearRegion, failure, read_linear, read_linear_region, sphere_error, write_linear_region};
use crate::{EngineError, Result, Session, commands::CommandSpec};
use photocraft_algo::sphere360::{
    self as sphere,
    finish::{Plate, PolePatch, PreparedPolePatch},
};
use photocraft_cms::{Builtin, Intent, Transform};
use photocraft_color::ColorMode;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    donor_document: u64,
    horizontal_fov: f64,
    #[serde(default)]
    patch: PolePatch,
}

fn enabled(s: &Session) -> std::result::Result<(), String> {
    crate::sphere360_finish_cmds::enabled(s)?;
    let state = s.active().ok_or("no document")?;
    let id = state.active_layer.ok_or("no active layer")?;
    if state.doc.effective_locks(id).transparency {
        return Err("unlock layer transparency before applying a pole repair".into());
    }
    Ok(())
}

fn transforms(s: &Session, doc: &photocraft_doc::Document) -> Result<(Transform, Transform)> {
    let profile = match &doc.icc_profile {
        Some(bytes) => crate::color_cmds::profile_from_bytes(bytes).map_err(failure)?,
        None => s.color.working(doc.mode),
    };
    if profile.color_space != crate::color_cmds::mode_space(doc.mode) {
        return Err(failure("embedded profile does not match the document colour mode"));
    }
    let linear = Builtin::LinearSrgb.profile();
    Ok((
        Transform::new(&profile, linear, Intent::RelativeColorimetric, false).map_err(failure)?,
        Transform::new(linear, &profile, Intent::RelativeColorimetric, false).map_err(failure)?,
    ))
}

fn patch_pole(s: &mut Session, value: &Value) -> Result<Value> {
    enabled(s).map_err(failure)?;
    let params: Params = serde_json::from_value(value.clone()).map_err(failure)?;
    let target = &s.active().ok_or(EngineError::NoDocument)?.doc;
    if target.id.0 == params.donor_document {
        return Err(failure("open a separate rectilinear repair image as the donor"));
    }
    let donor = s.documents().iter().find(|d| d.doc.id.0 == params.donor_document).ok_or_else(|| failure("repair source document is no longer open"))?;
    let donor_id = donor.active_layer.ok_or_else(|| failure("repair source has no active layer"))?;
    let layer = donor.doc.layer(donor_id).ok_or(EngineError::NoLayer(donor_id))?;
    let surface = layer.surface().ok_or_else(|| failure("repair source must be a raster layer"))?;
    if !matches!(donor.doc.mode, ColorMode::Rgb | ColorMode::Grayscale) || surface.format().mode != donor.doc.mode {
        return Err(failure("repair source must use matching RGB or grayscale document and layer modes"));
    }
    if layer.mask.is_some() || layer.vector_mask.is_some() {
        return Err(failure("apply donor layer masks to a copy before using it as a repair source"));
    }
    let (width, height) = (donor.doc.size.width, donor.doc.size.height);
    sphere::pixel_count(width, height).map_err(sphere_error)?;
    // Reject unusable geometry before allocating either full linear frame.
    let patch = params.patch;
    if width < 2
        || height < 2
        || !params.horizontal_fov.is_finite()
        || !(1.0..170.0).contains(&params.horizontal_fov)
        || !patch.radius_degrees.is_finite()
        || !(0.1..=60.0).contains(&patch.radius_degrees)
        || !patch.feather_degrees.is_finite()
        || patch.feather_degrees <= 0.0
        || patch.feather_degrees > patch.radius_degrees
    {
        return Err(failure("invalid repair dimensions, FOV, cap radius or feather"));
    }
    sphere::Orientation { yaw: patch.yaw, pitch: 0.0, roll: 0.0 }.validate().map_err(sphere_error)?;
    let tangent = (params.horizontal_fov.to_radians() * 0.5).tan();
    let usable = (tangent * (1.0 - 1.0 / f64::from(width))).min(tangent * f64::from(height) / f64::from(width) * (1.0 - 1.0 / f64::from(height)));
    if patch.radius_degrees.to_radians().tan() >= usable {
        return Err(failure("repair image FOV and resolution do not cover the entire cap; reduce the radius"));
    }
    let (target_to_linear, target_from_linear) = transforms(s, target)?;
    let (donor_to_linear, _) = transforms(s, &donor.doc)?;
    let donor = donor.doc.clone();
    crate::jobs::edit_job(
        s,
        "360 Pole Repair",
        move |doc, active, ctx| {
            ctx.check()?;
            let id = active.ok_or_else(|| failure("no active layer"))?;
            let (target_width, target_height) = (doc.size.width, doc.size.height);
            let source = donor.layer(donor_id).ok_or(EngineError::NoLayer(donor_id))?.surface().ok_or_else(|| failure("repair raster missing"))?;
            let plate_pixels = read_linear(source, width, height, &donor_to_linear, ctx, (0.0, 0.15))?;
            let surface = doc.layer_mut(id).ok_or(EngineError::NoLayer(id))?.surface_mut().ok_or_else(|| failure("target raster missing"))?;
            let plate = Plate { width, height, pixels: &plate_pixels, horizontal_fov: params.horizontal_fov };
            let prepared = PreparedPolePatch::new(&plate, patch, target_width, target_height).map_err(sphere_error)?;
            let cap = prepared.affected_rows();
            // Two RGBA32F working bands total at most 16 MiB. Native samples are
            // separately capped at 8 MiB; document tiles/history and donor add storage.
            let rows = (524_288 / target_width as usize).clamp(1, 128) as u32;
            for first in (cap.start..cap.end).step_by(rows as usize) {
                ctx.check()?;
                let end = first.saturating_add(rows).min(cap.end);
                let from = 0.15 + 0.85 * (first - cap.start) as f32 / cap.len() as f32;
                let to = 0.15 + 0.85 * (end - cap.start) as f32 / cap.len() as f32;
                let phase = to - from;
                let bounds = photocraft_geom::Rect::new(0, first as i32, target_width as i32, end as i32);
                let input = read_linear_region(surface, LinearRegion { bounds, progress: (from, from + phase * 0.3) }, &target_to_linear, ctx)?;
                let pixels = prepared
                    .apply_band(first, &input, &|p| {
                        ctx.progress(from + phase * (0.3 + p * 0.4), "Blending the pole repair");
                        !ctx.cancelled()
                    })
                    .map_err(sphere_error)?;
                write_linear_region(surface, LinearRegion { bounds, progress: (from + phase * 0.7, to) }, &target_from_linear, &input, &pixels, false, ctx)?;
            }
            Ok(json!({"layer":id.0,"scope":"active raster layer pixels","donor_document":donor.id.0,
            "donor_layer":donor_id.0,"width":target_width,"height":target_height,"patch":patch,
            "horizontal_fov":params.horizontal_fov,"processed_rows":cap.end-cap.start,"linear_band_storage_bytes":u64::from(target_width)*u64::from(rows.min(cap.end-cap.start))*32,"method":"supplied rectilinear clean plate; spherical linear-light source-over"}))
        },
        |v| v,
    )
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "sphere360.patchPole",
        label: "360 Pole Repair",
        menu: &[],
        shortcut: None,
        params: r#"{"donor_document":open_doc_id,"horizontal_fov":1..170,"patch":{"pole":"nadir|zenith","radius_degrees":0.1..60,"feather_degrees":0..radius,"yaw":degrees}}"#,
        enabled,
        run: patch_pole,
        journal: true,
    }]
}
