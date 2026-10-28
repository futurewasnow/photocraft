//! Native monoscopic panorama inspection. View changes never alter document pixels.
use crate::PhotocraftApp;
use egui::{Color32, ColorImage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{
    Arc, Weak,
    mpsc::{Receiver, TryRecvError},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub open: bool,
    pub yaw: f64,
    pub pitch: f64,
    pub fov: f64,
    pub denoise_strength: f32,
    pub pole_repair: PoleRepairState,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PoleRepairState {
    pub donor_document: Option<u64>,
    pub zenith: bool,
    pub radius: f64,
    pub feather: f64,
    pub horizontal_fov: f64,
    pub yaw: f64,
}
impl Default for PoleRepairState {
    fn default() -> Self {
        Self { donor_document: None, zenith: false, radius: 20., feather: 5., horizontal_fov: 90., yaw: 0. }
    }
}
impl PoleRepairState {
    fn valid(&self) -> bool {
        self.radius.is_finite()
            && (0.1..=60.).contains(&self.radius)
            && self.feather.is_finite()
            && self.feather > 0.
            && self.feather <= self.radius
            && self.horizontal_fov.is_finite()
            && (1.0..170.).contains(&self.horizontal_fov)
            && self.yaw.is_finite()
            && (-180.0..=180.).contains(&self.yaw)
    }
}
impl Default for State {
    fn default() -> Self {
        Self { open: false, yaw: 0., pitch: 0., fov: 90., denoise_strength: 0.8, pole_repair: PoleRepairState::default() }
    }
}
impl State {
    fn valid(&self) -> bool {
        self.yaw.is_finite()
            && self.pitch.is_finite()
            && self.fov.is_finite()
            && (-180.0..=180.0).contains(&self.yaw)
            && (-90.0..=90.0).contains(&self.pitch)
            && (30.0..=120.0).contains(&self.fov)
            && self.denoise_strength.is_finite()
            && (0.0..=1.0).contains(&self.denoise_strength)
            && self.pole_repair.valid()
    }
}
#[derive(Clone)]
struct Key {
    snapshot: Weak<photocraft_doc::Document>,
    revision: u64,
    display: u64,
}
impl Key {
    fn new(doc: &Arc<photocraft_doc::Document>, revision: u64, display: u64) -> Self {
        Self { snapshot: Arc::downgrade(doc), revision, display }
    }
}
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision && self.display == other.display && self.snapshot.ptr_eq(&other.snapshot)
    }
}
impl Eq for Key {}
#[derive(Default)]
pub(crate) struct Cache {
    key: Option<Key>,
    image: Option<ColorImage>,
    running: Option<(Key, Receiver<Result<ColorImage, String>>)>,
    error: Option<String>,
    texture: Option<egui::TextureHandle>,
    view: Option<[u64; 3]>,
    heading_job: Option<(photocraft_engine::jobs::JobId, photocraft_doc::DocId, u64)>,
}
impl Cache {
    fn finish_heading(&mut self, session: &photocraft_engine::Session, state: &mut State) {
        let Some((id, document, yaw)) = self.heading_job else {
            return;
        };
        let job = session.jobs_with_recent().into_iter().find(|job| job.id == id);
        if job.as_ref().is_some_and(|j| j.state == "running") {
            return;
        }
        self.heading_job = None;
        // A cancelled/failed edit must leave the view alone. Do not jump a different
        // document or override a heading the user changed while the edit was running.
        if job.is_some_and(|j| j.state == "done") && session.active().is_some_and(|s| s.doc.id == document) && state.yaw.to_bits() == yaw {
            state.yaw = 0.;
        }
    }

    fn accept(&mut self, key: Key, wanted: &Key, result: Result<ColorImage, String>) {
        if &key != wanted {
            return;
        }
        self.key = Some(key);
        self.view = None;
        match result {
            Ok(image) => {
                self.image = Some(image);
                self.error = None;
            }
            Err(error) => {
                self.image = None;
                self.error = Some(error);
            }
        }
    }
    fn prepare(&mut self, key: Key, doc: Arc<photocraft_doc::Document>, display: Arc<photocraft_engine::display_color::CanvasDisplay>, ctx: &egui::Context) {
        if self.key.as_ref() != Some(&key) {
            self.image = None;
            self.texture = None;
            self.error = None;
            self.view = None;
        }
        if let Some((running_key, rx)) = &self.running {
            let result = match rx.try_recv() {
                Ok(r) => Some(r),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("360 preview worker stopped".into())),
            };
            if let Some(result) = result {
                let running_key = running_key.clone();
                self.running = None;
                self.accept(running_key, &key, result);
            }
        }
        if self.key.as_ref() == Some(&key) || self.running.is_some() {
            return;
        }
        let compute = move || {
            let buffer = photocraft_compose::thumbnail_buffer(&doc, 2048);
            crate::canvas::display_image(Some(&display), &buffer)
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let repaint = ctx.clone();
            match std::thread::Builder::new().name("sphere360-preview".into()).spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(compute)).map_err(|_| "Could not render the 360 preview".to_string());
                let _ = tx.send(result);
                repaint.request_repaint();
            }) {
                Ok(_) => self.running = Some((key, rx)),
                Err(error) => self.accept(key.clone(), &key, Err(error.to_string())),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ctx;
            self.accept(key.clone(), &key, Ok(compute()));
        }
    }
}
pub fn handles(id: &str) -> bool {
    matches!(id, "view.sphere360" | "view.sphere360.look")
}
pub fn menu(app: &mut PhotocraftApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    if !handles(id) {
        return None;
    }
    let mut state = app.ui.sphere360.clone();
    if !state.valid() {
        state = State { open: state.open, ..State::default() };
    }
    let result = (|| {
        if id == "view.sphere360" {
            if !params.is_null() && params.as_object().is_none_or(|p| !p.is_empty()) {
                return Err("360 workspace toggle takes no parameters".into());
            }
            state.open = !state.open;
        } else {
            let object = params.as_object().ok_or("look parameters must be an object")?;
            for (name, value) in object {
                let value = value.as_f64().ok_or("view angles must be numbers")?;
                match name.as_str() {
                    "yaw" => state.yaw = value,
                    "pitch" => state.pitch = value,
                    "fov" => state.fov = value,
                    _ => return Err(format!("unknown 360 view parameter: {name}")),
                }
            }
            state.open = true;
        }
        if !state.valid() {
            return Err("yaw must be -180..180, pitch -90..90 and field of view 30..120 degrees".into());
        }
        app.ui.sphere360 = state;
        Ok(json!(app.ui.sphere360))
    })();
    Some(result)
}

/// Bounded display-only projection. Pixel edits continue through the native-channel engine.
fn project(source: &ColorImage, state: &State, width: usize, height: usize) -> Result<ColorImage, String> {
    let [w, h] = source.size;
    if !state.valid()
        || h == 0
        || w != h.saturating_mul(2)
        || w > 2048
        || source.pixels.len() != w.saturating_mul(h)
        || width == 0
        || height == 0
        || width > 1024
        || height > 1024
    {
        return Err("invalid 360 preview geometry".into());
    }
    let mut pixels = vec![Color32::TRANSPARENT; width * height];
    let (sy, cy) = state.yaw.to_radians().sin_cos();
    let (sp, cp) = state.pitch.to_radians().sin_cos();
    let scale = (state.fov.to_radians() * 0.5).tan();
    let sample = |x: i64, y: i64| {
        let (x, y) = if y < 0 {
            (x + w as i64 / 2, -y - 1)
        } else if y >= h as i64 {
            (x + w as i64 / 2, 2 * h as i64 - y - 1)
        } else {
            (x, y)
        };
        source.pixels.get(y as usize * w + x.rem_euclid(w as i64) as usize).copied().unwrap_or(Color32::TRANSPARENT)
    };
    let render = |(i, out): (usize, &mut Color32)| {
        let x = (2. * (i % width) as f64 + 1. - width as f64) / width as f64 * scale;
        let y = (height as f64 - 2. * (i / width) as f64 - 1.) / width as f64 * scale;
        let z = cp - sp * y;
        let dy = cp * y + sp;
        let dx = cy * x + sy * z;
        let dz = cy * z - sy * x;
        let longitude = dx.atan2(dz);
        let latitude = (dy / (1. + x * x + y * y).sqrt()).clamp(-1., 1.).asin();
        let u = (longitude / std::f64::consts::TAU + 0.5) * w as f64 - 0.5;
        let v = (0.5 - latitude / std::f64::consts::PI) * h as f64 - 0.5;
        let ix = u.floor() as i64;
        let iy = v.floor() as i64;
        let fx = u - u.floor();
        let fy = v - v.floor();
        let corners = [sample(ix, iy), sample(ix + 1, iy), sample(ix, iy + 1), sample(ix + 1, iy + 1)];
        let weights = [(1. - fx) * (1. - fy), fx * (1. - fy), (1. - fx) * fy, fx * fy];
        let channels: [u8; 4] = std::array::from_fn(|c| {
            corners.iter().zip(weights).map(|(p, a)| f64::from(p.to_array().get(c).copied().unwrap_or(0)) * a).sum::<f64>().round().clamp(0., 255.) as u8
        });
        *out = Color32::from_rgba_premultiplied(channels[0], channels[1], channels[2], channels[3]);
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        use rayon::prelude::*;
        pixels.par_iter_mut().enumerate().for_each(render);
    }
    #[cfg(target_arch = "wasm32")]
    pixels.iter_mut().enumerate().for_each(render);
    Ok(ColorImage::new([width, height], pixels))
}

pub fn windows(app: &mut PhotocraftApp, ctx: &egui::Context) {
    app.sphere360_cache.finish_heading(&app.session, &mut app.ui.sphere360);
    if !app.ui.sphere360.open {
        return;
    }
    if !app.ui.sphere360.valid() {
        app.ui.sphere360 = State { open: true, ..State::default() };
    }
    let mut cache = std::mem::take(&mut app.sphere360_cache);
    let active = app.session.active().map(|s| (s.doc.clone(), s.revision));
    let valid = active.as_ref().is_some_and(|(d, _)| d.size.height > 0 && u64::from(d.size.width) == 2 * u64::from(d.size.height));
    if let Some((doc, revision)) = &active
        && valid
    {
        match app.session.color.canvas_display_for(doc, app.session.color.main_display) {
            Ok(display) => cache.prepare(Key::new(doc, *revision, display.key), doc.clone(), display, ctx),
            Err(error) => {
                cache.key = None;
                cache.image = None;
                cache.texture = None;
                cache.error = Some(error.to_string());
            }
        }
    }
    let mut open = true;
    let mut edit = None;
    let mut denoise = false;
    let mut patch = false;
    let mut state = app.ui.sphere360.clone();
    egui::Window::new(tl!("360° Workspace"))
        .id(egui::Id::new("sphere360-workspace"))
        .open(&mut open)
        .default_width(580.)
        .default_height(900.)
        .resizable(true)
        .vscroll(true)
        .show(ctx, |ui| {
            ui.label(tl!("Monoscopic 360 × 180° · equirectangular"));
            if !valid {
                ui.weak(tl!("Open a full-sphere 2:1 panorama to inspect it here."));
                return;
            }
            ui.horizontal_wrapped(|ui| {
                for (label, yaw, pitch) in [("Front", 0., 0.), ("Seam", 180., 0.), ("Nadir", 0., -90.), ("Zenith", 0., 90.)] {
                    if ui.button(tl!(label)).clicked() {
                        state.yaw = yaw;
                        state.pitch = pitch;
                    }
                }
                if ui.button(tl!("Reset view")).clicked() {
                    state = State { open: true, denoise_strength: state.denoise_strength, pole_repair: state.pole_repair.clone(), ..State::default() };
                }
            });
            ui.add(egui::Slider::new(&mut state.yaw, -180.0..=180.0).text(tl!("Look left / right")));
            ui.add(egui::Slider::new(&mut state.pitch, -90.0..=90.0).text(tl!("Look up / down")));
            ui.add(egui::Slider::new(&mut state.fov, 30.0..=120.0).text(tl!("Field of view")));
            if let Some(error) = &cache.error {
                ui.label(error);
                if ui.button(tl!("Retry preview")).clicked() {
                    cache.key = None;
                    cache.view = None;
                    ctx.request_repaint();
                }
            }
            if cache.running.is_some() {
                ui.spinner();
                ui.weak(tl!("Updating preview…"));
                ctx.request_repaint_after(std::time::Duration::from_millis(40));
            }
            if let Some(image) = &cache.image {
                let key = [state.yaw.to_bits(), state.pitch.to_bits(), state.fov.to_bits()];
                if cache.view != Some(key) {
                    match project(image, &state, 512, 288) {
                        Ok(image) => {
                            if let Some(texture) = &mut cache.texture {
                                texture.set(image, egui::TextureOptions::LINEAR);
                            } else {
                                cache.texture = Some(ctx.load_texture("sphere360-view", image, egui::TextureOptions::LINEAR));
                            }
                            cache.view = Some(key);
                        }
                        Err(error) => cache.error = Some(error),
                    }
                }
                if let Some(texture) = &cache.texture {
                    let width = ui.available_width().clamp(120., 900.);
                    let response = ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(width, width * 9. / 16.)).sense(egui::Sense::drag()));
                    if response.dragged() {
                        let delta = ui.input(|i| i.pointer.delta());
                        state.yaw = (state.yaw - f64::from(delta.x / width) * state.fov + 180.).rem_euclid(360.) - 180.;
                        state.pitch = (state.pitch + f64::from(delta.y / width) * state.fov).clamp(-90., 90.);
                        ctx.request_repaint();
                    }
                }
            }
            ui.weak(tl!("Display preview · up to 2048 px source · drag to look around"));
            ui.separator();
            ui.label(tl!("Active raster layer — lossless horizontal shift"));
            let reason = app.session.disabled_reason("sphere360.yaw");
            ui.add_enabled_ui(reason.is_none(), |ui| {
                ui.horizontal_wrapped(|ui| {
                    let width = active.as_ref().map_or(0, |(d, _)| d.size.width);
                    if ui.button(tl!("Set heading as front")).clicked() {
                        edit = Some((-(state.yaw / 360. * f64::from(width)).round() as i64, true));
                    }
                    if ui.button(tl!("Move seam 180°")).clicked() {
                        edit = Some((i64::from(width / 2), false));
                    }
                });
            });
            if let Some(reason) = reason {
                ui.weak(reason);
            }
            ui.weak(tl!("Edits affect this layer only. Undo restores its previous pixels."));
            ui.separator();
            ui.label(tl!("360 denoise · active RGB or grayscale layer"));
            ui.add(egui::Slider::new(&mut state.denoise_strength, 0.0..=1.0).text(tl!("Noise reduction")));
            let reason = app.session.disabled_reason("sphere360.denoise");
            if ui.add_enabled(reason.is_none() && state.denoise_strength > 0.0, egui::Button::new(tl!("Auto denoise sphere"))).clicked() {
                denoise = true;
            }
            if let Some(reason) = reason {
                ui.weak(reason);
            }
            ui.weak(tl!("Estimates noise and protects edges across the seam and poles. Review fine detail; undo is available."));
            ui.separator();
            egui::CollapsingHeader::new(tl!("Nadir / zenith repair")).id_salt("sphere360-pole-repair").show(ui, |ui| {
                ui.weak(tl!("Open a rectilinear clean plate in another tab. Its active raster pixels supply the repair."));
                let repair = &mut state.pole_repair;
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut repair.zenith, false, tl!("Nadir"));
                    ui.selectable_value(&mut repair.zenith, true, tl!("Zenith"));
                    if ui.button(tl!("Inspect pole")).clicked() {
                        state.pitch = if repair.zenith { 90. } else { -90. };
                        state.yaw = repair.yaw;
                    }
                });
                let documents: Vec<_> = app
                    .session
                    .documents()
                    .iter()
                    .filter(|d| active.as_ref().is_none_or(|(target, _)| target.id != d.doc.id))
                    .map(|d| (d.doc.id.0, d.doc.name.clone()))
                    .collect();
                let selected = documents.iter().find(|(id, _)| Some(*id) == repair.donor_document);
                let donor_open = selected.is_some();
                egui::ComboBox::from_id_salt("sphere360-pole-donor")
                    .selected_text(selected.map_or_else(|| tl!("Choose repair image"), |(_, name)| name.as_str()))
                    .show_ui(ui, |ui| {
                        for (id, name) in &documents {
                            ui.selectable_value(&mut repair.donor_document, Some(*id), name);
                        }
                    });
                ui.add(egui::Slider::new(&mut repair.horizontal_fov, 1.0..=169.0).text(tl!("Source horizontal FOV")));
                ui.add(egui::Slider::new(&mut repair.yaw, -180.0..=180.0).text(tl!("Repair rotation")));
                ui.add(egui::Slider::new(&mut repair.radius, 0.1..=60.0).text(tl!("Cap radius")));
                repair.feather = repair.feather.min(repair.radius);
                ui.add(egui::Slider::new(&mut repair.feather, 0.1..=repair.radius).text(tl!("Edge feather")));
                let reason = app.session.disabled_reason("sphere360.patchPole");
                if ui.add_enabled(donor_open && reason.is_none(), egui::Button::new(tl!("Apply pole repair"))).clicked() {
                    patch = true;
                }
                if let Some(reason) = reason {
                    ui.weak(reason);
                }
                ui.weak(tl!("FOV must match the source projection. Colour-managed blend, one undo step. No automatic object removal or video tracking."));
            });
        });
    state.open = open;
    app.ui.sphere360 = state;
    if patch {
        let repair = &app.ui.sphere360.pole_repair;
        let params = json!({"donor_document":repair.donor_document,"horizontal_fov":repair.horizontal_fov,
            "patch":{"pole":if repair.zenith {"zenith"} else {"nadir"},"radius_degrees":repair.radius,
                "feather_degrees":repair.feather,"yaw":repair.yaw}});
        match app.run("sphere360.patchPole", params) {
            Ok(_) => cache.error = None,
            Err(error) => cache.error = Some(error),
        }
    }
    if denoise && let Err(error) = app.run("sphere360.denoise", json!({"strength":app.ui.sphere360.denoise_strength})) {
        cache.error = Some(error);
    }
    if let Some((columns, reset)) = edit {
        match app.run("sphere360.yaw", json!({"columns":columns})) {
            Ok(result) => {
                if reset {
                    if result.get("pending") == Some(&Value::Bool(true)) {
                        if let Some(job) = result.get("job").and_then(Value::as_u64)
                            && let Some((doc, _)) = &active
                        {
                            cache.heading_job = Some((photocraft_engine::jobs::JobId(job), doc.id, app.ui.sphere360.yaw.to_bits()));
                        }
                    } else {
                        app.ui.sphere360.yaw = 0.;
                    }
                }
            }
            Err(error) => cache.error = Some(error),
        }
    }
    app.sphere360_cache = cache;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn denoise_state_is_backward_compatible_and_bounded() {
        let old: State = serde_json::from_value(json!({"open":true,"yaw":20,"pitch":0,"fov":90})).unwrap();
        assert_eq!(old.denoise_strength, 0.8);
        assert!(old.valid());
        assert_eq!(old.pole_repair, PoleRepairState::default());
        for value in [-1.0, 2.0, f32::NAN] {
            assert!(!State { denoise_strength: value, ..old.clone() }.valid());
        }
        let stored = State { denoise_strength: 0.35, ..old };
        assert_eq!(serde_json::from_value::<State>(json!(stored)).unwrap(), stored);
    }
    #[test]
    fn pole_repair_state_is_bounded_and_persistent() {
        let state = State {
            pole_repair: PoleRepairState { donor_document: Some(42), zenith: true, radius: 30., feather: 7., horizontal_fov: 100., yaw: 25. },
            ..State::default()
        };
        assert!(state.valid());
        assert_eq!(serde_json::from_value::<State>(json!(state)).unwrap(), state);
        for invalid in [
            PoleRepairState { feather: 0., ..state.pole_repair.clone() },
            PoleRepairState { feather: 40., ..state.pole_repair.clone() },
            PoleRepairState { horizontal_fov: 170., ..state.pole_repair.clone() },
            PoleRepairState { yaw: f64::NAN, ..state.pole_repair.clone() },
        ] {
            assert!(!State { pole_repair: invalid, ..state.clone() }.valid());
        }
    }
    #[test]
    fn panorama_view_handles_seam_poles_and_transparency() {
        let (w, h) = (512, 256);
        let pixels = (0..w * h)
            .map(|i| {
                let lon = ((i % w) as f64 + 0.5) / w as f64 * std::f64::consts::TAU - std::f64::consts::PI;
                let lat = (0.5 - ((i / w) as f64 + 0.5) / h as f64) * std::f64::consts::PI;
                let d = [lat.cos() * lon.sin(), lat.sin(), lat.cos() * lon.cos()];
                let c = d.map(|v| ((0.5 + 0.4 * v) * 255.).round() as u8);
                Color32::from_rgb(c[0], c[1], c[2])
            })
            .collect();
        let source = ColorImage::new([w, h], pixels);
        for (yaw, pitch, expected) in [
            (0., 0., [128, 128, 230]),
            (180., 0., [128, 128, 26]),
            (-180., 0., [128, 128, 26]),
            (90., 0., [230, 128, 128]),
            (0., 90., [128, 230, 128]),
            (0., -90., [128, 26, 128]),
        ] {
            let state = State { yaw, pitch, ..State::default() };
            let result = project(&source, &state, 1, 1).unwrap();
            let p = result.pixels[0].to_array();
            for c in 0..3 {
                assert!((i16::from(p[c]) - expected[c]).abs() <= 2, "yaw={yaw} pitch={pitch}: {p:?}");
            }
            assert_eq!(p[3], 255);
        }
        let translucent = Color32::from_rgba_premultiplied(40, 20, 10, 80);
        let source = ColorImage::new([8, 4], vec![translucent; 32]);
        let pole = project(&source, &State { yaw: 180., pitch: 90., ..State::default() }, 35, 21).unwrap();
        assert!(pole.pixels.iter().all(|p| *p == translucent));
        assert!(project(&source, &State { fov: f64::NAN, ..State::default() }, 1, 1).is_err());
        assert!(project(&source, &State::default(), 0, 1).is_err());
        assert!(project(&ColorImage::new([4, 4], vec![Color32::BLACK; 16]), &State::default(), 1, 1).is_err());
    }
    #[test]
    fn view_commands_are_atomic_serialized_and_document_independent() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        let ctx = egui::Context::default();
        let closed_menu = crate::native_menu::state_hash(&app, None);
        crate::menus::invoke(&mut app, &ctx, "view.sphere360", Value::Null).unwrap();
        assert!(app.ui.sphere360.open);
        let open_menu = crate::native_menu::state_hash(&app, None);
        assert_ne!(open_menu, closed_menu, "automation must refresh the checked menu item without pointer input");
        crate::menus::invoke(&mut app, &ctx, "view.sphere360.look", json!({"yaw":180,"pitch":-90,"fov":70})).unwrap();
        assert_eq!(crate::native_menu::state_hash(&app, None), open_menu, "looking around should reuse the menu cache");
        let before = app.ui.sphere360.clone();
        assert_eq!(crate::control::inspect(&app, &ctx)["sphere360"], serde_json::to_value(&before).unwrap());
        for bad in [json!({"yaw":0,"pitch":100}), json!({"yaw":"nan"}), json!({"unknown":2}), json!([])] {
            assert!(crate::menus::invoke(&mut app, &ctx, "view.sphere360.look", bad).is_err());
            assert_eq!(app.ui.sphere360, before);
        }
        let roundtrip: State = serde_json::from_value(serde_json::to_value(&before).unwrap()).unwrap();
        assert_eq!(roundtrip, before);
        assert_eq!(serde_json::from_value::<State>(json!({})).unwrap(), State::default());
        assert!(app.session.active().is_none());
        assert!(crate::menus::menu_items(&app).iter().any(|item| item.id == "view.sphere360" && item.enabled && item.checked == Some(true)));
        app.ui.sphere360 = State { fov: 0., ..State::default() };
        crate::menus::invoke(&mut app, &ctx, "view.sphere360", Value::Null).unwrap();
        assert_eq!(app.ui.sphere360, State { open: true, ..State::default() }, "invalid saved angles must not trap a closed panel");
    }
    #[test]
    fn old_document_preview_cannot_replace_current_document() {
        let original = Arc::new(photocraft_doc::Document::new(
            "same persisted identity",
            photocraft_geom::Size::new(2, 1),
            photocraft_color::ColorMode::Rgb,
            photocraft_color::SampleType::U8,
        ));
        let reopened = Arc::new((*original).clone());
        assert_eq!(original.id, reopened.id);
        let old = Key::new(&original, 0, 0);
        let current = Key::new(&reopened, 0, 0);
        let mut cache = Cache::default();
        cache.accept(old.clone(), &current, Ok(ColorImage::new([2, 1], vec![Color32::RED; 2])));
        assert!(cache.image.is_none());
        cache.accept(current.clone(), &current, Ok(ColorImage::new([2, 1], vec![Color32::GREEN; 2])));
        cache.accept(old, &current, Err("stale failure".into()));
        assert_eq!(cache.image.as_ref().unwrap().pixels[0], Color32::GREEN);
        assert!(cache.error.is_none());
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn heading_view_resets_only_after_success_on_the_same_view() {
        use photocraft_engine::jobs::Started;
        for (cancel, changed_view, changed_document) in [(true, false, false), (false, false, false), (false, true, false), (false, false, true)] {
            let mut session = photocraft_engine::Session::new();
            session.execute("file.new", json!({"width":16,"height":8,"background":"transparent"})).unwrap();
            let document = session.active().unwrap().doc.id;
            let Started::Job(job) = session.start("sphere360.yaw", json!({"columns":8})).unwrap() else {
                panic!("native yaw should start a background job");
            };
            let mut state = State { yaw: 180., ..State::default() };
            let mut cache = Cache { heading_job: Some((job, document, state.yaw.to_bits())), ..Cache::default() };
            cache.finish_heading(&session, &mut state);
            assert_eq!(state.yaw, 180.);
            if changed_view {
                state.yaw = 45.;
            }
            if cancel {
                assert!(session.cancel_job(job));
            } else {
                session.wait_job(job).unwrap();
            }
            if changed_document {
                session.execute("file.new", json!({"width":16,"height":8,"background":"transparent"})).unwrap();
            }
            cache.finish_heading(&session, &mut state);
            assert_eq!(
                state.yaw,
                if changed_view {
                    45.
                } else if cancel || changed_document {
                    180.
                } else {
                    0.
                }
            );
            assert!(cache.heading_job.is_none());
        }
    }
}
