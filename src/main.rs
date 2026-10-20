//! sheetview — a furniture-free, touch-first PDF viewer for sheet music.
//!
//! Library: grid of first-page thumbnails for every PDF in the folder.
//! Viewer:  tap right half = next page, left half = previous page,
//!          swipe left/right or tap the top-centre strip = back to library.
//! Keys:    →/↓/PgDn/Space next, ←/↑/PgUp prev, Home/End, Esc/Back back (quit from library).
//! The window keeps itself fullscreen.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use eframe::egui::{self, Color32, ColorImage, Pos2, Rect, Sense, TextureHandle, TextureOptions, Vec2};
use mupdf::{Colorspace, Document, Matrix};

const RENDER_WORKERS: usize = 2;
const THUMB_WORKERS: usize = 3;
const THUMB_W: f32 = 300.0;
const TILE_W: f32 = 170.0;
/// Horizontal travel (points) after which a press counts as a "back" swipe.
const SWIPE_DIST: f32 = 120.0;
const BG: Color32 = Color32::from_gray(24);
const UV: Rect = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));

fn main() -> eframe::Result {
    // Argument may be a folder (library) or a single PDF (opened directly, library = its folder).
    let arg = std::env::args().nth(1).map(PathBuf::from);
    let (dir, initial) = match arg {
        Some(p) if p.is_file() => (p.parent().map(Path::to_path_buf).unwrap_or_default(), Some(p)),
        Some(p) => (p, None),
        None => (std::env::current_dir().unwrap(), None),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Sheet music")
            .with_fullscreen(true),
        ..Default::default()
    };
    eframe::run_native(
        "sheetview",
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc.egui_ctx.clone(), dir);
            if let Some(p) = initial {
                app.open(p);
            }
            Ok(Box::new(app))
        }),
    )
}

// ---------------------------------------------------------------- utilities

fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache"));
    let dir = base.join("sheetview");
    let _ = std::fs::create_dir_all(dir.join("thumbs"));
    dir
}

/// Strip download-site noise from file names to get a readable title.
fn title_of(path: &Path) -> String {
    let mut s = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    for junk in [
        "(z-library.sk, 1lib.sk, z-lib.sk)",
        "_z_library_sk,_1lib_sk,",
        "z_library_sk,_1lib_sk,",
    ] {
        s = s.replace(junk, "");
    }
    s.replace('_', " ").trim().trim_end_matches("English").trim().to_string()
}

fn pixmap_to_image(pix: &mupdf::Pixmap) -> ColorImage {
    let (w, h, n) = (pix.width() as usize, pix.height() as usize, pix.n() as usize);
    let stride = pix.stride() as usize;
    let samples = pix.samples();
    if stride == w * 3 && n == 3 {
        return ColorImage::from_rgb([w, h], &samples[..w * h * 3]);
    }
    let mut rgb = Vec::with_capacity(w * h * 3);
    for row in samples.chunks(stride).take(h) {
        for px in row[..w * n].chunks(n) {
            rgb.extend_from_slice(&px[..3]);
        }
    }
    ColorImage::from_rgb([w, h], &rgb)
}

/// Render `page_no` scaled to fit inside `max_w` x `max_h` pixels.
fn render_fit(doc: &Document, page_no: i32, max_w: f32, max_h: f32) -> Option<ColorImage> {
    let page = doc.load_page(page_no).ok()?;
    let b = page.bounds().ok()?;
    let (pw, ph) = (b.x1 - b.x0, b.y1 - b.y0);
    let s = (max_w / pw).min(max_h / ph);
    let pix = page
        .to_pixmap(&Matrix::new_scale(s, s), &Colorspace::device_rgb(), false, false)
        .ok()?;
    Some(pixmap_to_image(&pix))
}

// ------------------------------------------------------------ page renderer

/// What the viewer currently wants rendered, in priority order.
struct Wanted {
    generation: u64,
    path: PathBuf,
    size: (f32, f32),
    queue: VecDeque<i32>,
    in_flight: HashSet<i32>,
}

struct Rendered {
    generation: u64,
    page: i32,
    image: ColorImage,
}

struct Renderer {
    wanted: Arc<(Mutex<Wanted>, Condvar)>,
    rx: Receiver<Rendered>,
}

impl Renderer {
    fn new(ctx: egui::Context) -> Self {
        let wanted = Arc::new((
            Mutex::new(Wanted {
                generation: 0,
                path: PathBuf::new(),
                size: (0.0, 0.0),
                queue: VecDeque::new(),
                in_flight: HashSet::new(),
            }),
            Condvar::new(),
        ));
        let (tx, rx) = channel();
        for _ in 0..RENDER_WORKERS {
            let (wanted, tx, ctx) = (wanted.clone(), tx.clone(), ctx.clone());
            thread::spawn(move || render_worker(wanted, tx, ctx));
        }
        Renderer { wanted, rx }
    }

    /// Replace the work queue. `skip` holds pages the UI already has.
    fn request(&self, generation: u64, path: &Path, size: (f32, f32), pages: &[i32], skip: &HashSet<i32>) {
        let (lock, cv) = &*self.wanted;
        let mut w = lock.lock().unwrap();
        if w.generation != generation {
            w.in_flight.clear();
        }
        w.generation = generation;
        w.path = path.to_path_buf();
        w.size = size;
        let queue: VecDeque<i32> = pages
            .iter()
            .copied()
            .filter(|p| !skip.contains(p) && !w.in_flight.contains(p))
            .collect();
        if queue != w.queue {
            w.queue = queue;
            cv.notify_all();
        }
    }
}

fn render_worker(wanted: Arc<(Mutex<Wanted>, Condvar)>, tx: Sender<Rendered>, ctx: egui::Context) {
    let mut open: Option<(PathBuf, Document)> = None;
    loop {
        let (generation, path, size, page) = {
            let (lock, cv) = &*wanted;
            let mut w = cv.wait_while(lock.lock().unwrap(), |w| w.queue.is_empty()).unwrap();
            let page = w.queue.pop_front().unwrap();
            w.in_flight.insert(page);
            (w.generation, w.path.clone(), w.size, page)
        };
        if open.as_ref().is_none_or(|(p, _)| *p != path) {
            open = Document::open(path.as_path()).ok().map(|d| (path.clone(), d));
        }
        let image = open.as_ref().and_then(|(_, doc)| render_fit(doc, page, size.0, size.1));
        {
            let mut w = wanted.0.lock().unwrap();
            if w.generation == generation {
                w.in_flight.remove(&page);
            }
        }
        if let Some(image) = image {
            let _ = tx.send(Rendered { generation, page, image });
            ctx.request_repaint();
        }
    }
}

// --------------------------------------------------------------- thumbnails

fn spawn_thumbnailers(ctx: egui::Context, files: Vec<PathBuf>) -> Receiver<(usize, ColorImage)> {
    let (tx, rx) = channel();
    let files = Arc::new(files);
    let next = Arc::new(AtomicUsize::new(0));
    let thumbs = cache_dir().join("thumbs");
    for _ in 0..THUMB_WORKERS {
        let (files, next, tx, ctx, thumbs) = (files.clone(), next.clone(), tx.clone(), ctx.clone(), thumbs.clone());
        thread::spawn(move || {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = files.get(i) else { break };
                if let Some(img) = thumbnail(path, &thumbs) {
                    let _ = tx.send((i, img));
                    ctx.request_repaint();
                }
            }
        });
    }
    rx
}

fn thumbnail(path: &Path, dir: &Path) -> Option<ColorImage> {
    let meta = std::fs::metadata(path).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (path, meta.len(), meta.modified().ok()).hash(&mut h);
    let cached = dir.join(format!("{:016x}.png", h.finish()));
    if let Ok(img) = image::open(&cached) {
        let img = img.to_rgb8();
        return Some(ColorImage::from_rgb([img.width() as _, img.height() as _], img.as_raw()));
    }
    let doc = Document::open(path).ok()?;
    let img = render_fit(&doc, 0, THUMB_W, THUMB_W * 1.6)?;
    let rgb: Vec<u8> = img.pixels.iter().flat_map(|c| [c.r(), c.g(), c.b()]).collect();
    let _ = image::save_buffer(&cached, &rgb, img.size[0] as _, img.size[1] as _, image::ColorType::Rgb8);
    Some(img)
}

// ---------------------------------------------------------------------- app

struct Viewer {
    path: PathBuf,
    page_count: i32,
    page: i32,
    generation: u64,
    size: (f32, f32),
    pages: HashMap<i32, TextureHandle>,
    /// Where the current touch started and the page shown before it turned the page.
    press: Option<(Pos2, i32)>,
}

struct App {
    files: Vec<PathBuf>,
    titles: Vec<String>,
    thumbs: Vec<Option<TextureHandle>>,
    thumb_rx: Receiver<(usize, ColorImage)>,
    renderer: Renderer,
    viewer: Option<Viewer>,
    generation: u64,
    positions: HashMap<PathBuf, i32>,
}

impl App {
    fn new(ctx: egui::Context, dir: PathBuf) -> Self {
        let mut files: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")))
                    .map(|p| (title_of(&p), p))
                    .collect()
            })
            .unwrap_or_default();
        files.sort_by_key(|(t, _)| t.to_lowercase());
        let (titles, files): (Vec<String>, Vec<PathBuf>) = files.into_iter().unzip();

        let positions = std::fs::read_to_string(cache_dir().join("positions.tsv"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let (n, p) = l.split_once('\t')?;
                Some((PathBuf::from(p), n.parse().ok()?))
            })
            .collect();

        App {
            thumbs: vec![None; files.len()],
            thumb_rx: spawn_thumbnailers(ctx.clone(), files.clone()),
            renderer: Renderer::new(ctx),
            files,
            titles,
            viewer: None,
            generation: 0,
            positions,
        }
    }

    fn save_positions(&self) {
        let body: String = self.positions.iter().map(|(p, n)| format!("{n}\t{}\n", p.display())).collect();
        let _ = std::fs::write(cache_dir().join("positions.tsv"), body);
    }

    fn open(&mut self, path: PathBuf) {
        let Some(page_count) = Document::open(path.as_path()).ok().and_then(|d| d.page_count().ok()) else { return };
        if page_count < 1 {
            return;
        }
        let page = self.positions.get(&path).copied().unwrap_or(0).clamp(0, page_count - 1);
        self.generation += 1;
        self.viewer = Some(Viewer {
            path,
            page_count,
            page,
            generation: self.generation,
            size: (0.0, 0.0),
            pages: HashMap::new(),
            press: None,
        });
    }

    fn library(&mut self, ui: &mut egui::Ui) {
        while let Ok((i, img)) = self.thumb_rx.try_recv() {
            self.thumbs[i] = Some(ui.ctx().load_texture(format!("thumb{i}"), img, TextureOptions::LINEAR));
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Q)) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0.0, BG);

        // A single close button is the only chrome; fullscreen otherwise has no exit for touch.
        let close = Rect::from_min_size(Pos2::new(full.right() - 56.0, full.top() + 8.0), Vec2::splat(48.0));
        let resp = ui.interact(close, egui::Id::unique("close"), Sense::click());
        let (c, d) = (close.center(), 10.0);
        let stroke = egui::Stroke::new(2.5, Color32::from_gray(150));
        ui.painter().line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], stroke);
        ui.painter().line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], stroke);
        if resp.clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        let gap = 18.0;
        let width = full.width() - 2.0 * gap;
        let cols = ((width + gap) / (TILE_W + gap)).floor().max(1.0) as usize;
        let tile_w = (width - gap * (cols as f32 - 1.0)) / cols as f32;
        let thumb_h = tile_w * 1.35;
        let tile_h = thumb_h + 48.0;
        let rows = self.files.len().div_ceil(cols);
        let mut clicked = None;

        let area = Rect::from_min_max(Pos2::new(full.left(), full.top() + 64.0), full.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let (rect, _) = ui.allocate_exact_size(
                    Vec2::new(full.width(), rows as f32 * (tile_h + gap) + gap),
                    Sense::hover(),
                );
                for (i, title) in self.titles.iter().enumerate() {
                    let (r, c) = (i / cols, i % cols);
                    let min = rect.min + Vec2::new(gap + c as f32 * (tile_w + gap), r as f32 * (tile_h + gap));
                    let tile = Rect::from_min_size(min, Vec2::new(tile_w, tile_h));
                    if !ui.is_rect_visible(tile) {
                        continue;
                    }
                    if ui.interact(tile, egui::Id::unique(("tile", i)), Sense::click()).clicked() {
                        clicked = Some(i);
                    }
                    let thumb_box = Rect::from_min_size(min, Vec2::new(tile_w, thumb_h));
                    match &self.thumbs[i] {
                        Some(tex) => {
                            let size = tex.size_vec2();
                            let s = (tile_w / size.x).min(thumb_h / size.y);
                            let r = Rect::from_center_size(
                                Pos2::new(thumb_box.center().x, thumb_box.top() + size.y * s / 2.0),
                                size * s,
                            );
                            ui.painter().image(tex.id(), r, UV, Color32::WHITE);
                        }
                        None => {
                            ui.painter().rect_filled(thumb_box, 2.0, Color32::from_gray(50));
                        }
                    }
                    let mut job = egui::text::LayoutJob::simple(
                        title.clone(),
                        egui::FontId::proportional(14.0),
                        Color32::from_gray(220),
                        tile_w,
                    );
                    job.wrap.max_rows = 2;
                    job.halign = egui::Align::Center;
                    let galley = ui.painter().layout_job(job);
                    ui.painter().galley(Pos2::new(tile.center().x, thumb_box.bottom() + 6.0), galley, Color32::WHITE);
                }
            });
        });
        if let Some(i) = clicked {
            self.open(self.files[i].clone());
        }
    }

    fn view(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        let ppp = ui.ctx().pixels_per_point();
        let size = ((full.width() * ppp).round(), (full.height() * ppp).round());
        let mut v = self.viewer.take().unwrap();

        if v.size != size {
            // Window resized or rotated: everything rendered so far is the wrong size.
            self.generation += 1;
            v.generation = self.generation;
            v.size = size;
            v.pages.clear();
        }
        while let Ok(r) = self.renderer.rx.try_recv() {
            if r.generation == v.generation {
                let tex = ui.ctx().load_texture(format!("p{}", r.page), r.image, TextureOptions::LINEAR);
                v.pages.insert(r.page, tex);
            }
        }

        // Input: act on press (not release) so turns feel instant.
        // Esc, the Back key (TouchUp back button) or mouse back button return to the library.
        let mut back = false;
        let mut target = v.page;
        ui.input(|i| {
            use egui::Key::*;
            if i.pointer.any_pressed()
                && let Some(p) = i.pointer.press_origin()
            {
                let top_centre =
                    p.y < full.top() + full.height() * 0.12 && (p.x - full.center().x).abs() < full.width() * 0.2;
                if top_centre {
                    back = true;
                } else {
                    v.press = Some((p, v.page));
                    target += if p.x >= full.center().x { 1 } else { -1 };
                }
            }
            // A press that becomes a horizontal swipe undoes its page turn and goes back.
            if let Some((origin, page_before)) = v.press {
                if let Some(pos) = i.pointer.latest_pos() {
                    let d = pos - origin;
                    if d.x.abs() > SWIPE_DIST.min(full.width() * 0.15) && d.x.abs() > 2.0 * d.y.abs() {
                        target = page_before;
                        back = true;
                        v.press = None;
                    }
                }
                if !i.pointer.any_down() {
                    v.press = None;
                }
            }
            back |= i.key_pressed(BrowserBack) || i.pointer.button_pressed(egui::PointerButton::Extra1);
            for k in [ArrowRight, ArrowDown, PageDown, Space, Enter] {
                if i.key_pressed(k) {
                    target += 1;
                }
            }
            for k in [ArrowLeft, ArrowUp, PageUp, Backspace] {
                if i.key_pressed(k) {
                    target -= 1;
                }
            }
            if i.key_pressed(Home) {
                target = 0;
            }
            if i.key_pressed(End) {
                target = v.page_count - 1;
            }
            back |= i.key_pressed(Escape);
        });
        let target = target.clamp(0, v.page_count - 1);
        if target != v.page {
            v.page = target;
            self.positions.insert(v.path.clone(), v.page);
            self.save_positions();
        }

        // Keep a window of pages around the current one; prefetch ahead first.
        let p = v.page;
        v.pages.retain(|&k, _| k >= p - 3 && k <= p + 5);
        let wanted: Vec<i32> = [p, p + 1, p + 2, p - 1, p + 3, p + 4, p - 2]
            .into_iter()
            .filter(|&k| k >= 0 && k < v.page_count)
            .collect();
        let have: HashSet<i32> = v.pages.keys().copied().collect();
        self.renderer.request(v.generation, &v.path, size, &wanted, &have);

        ui.painter().rect_filled(full, 0.0, BG);
        if let Some(tex) = v.pages.get(&p) {
            // Draw at exact physical pixel size, snapped, so the page is never resampled.
            let [w, h] = tex.size();
            let x = ((size.0 - w as f32) / 2.0).floor() / ppp;
            let y = ((size.1 - h as f32) / 2.0).floor() / ppp;
            let r = Rect::from_min_size(full.min + Vec2::new(x, y), Vec2::new(w as f32 / ppp, h as f32 / ppp));
            ui.painter().image(tex.id(), r, UV, Color32::WHITE);
        }

        if !back {
            self.viewer = Some(v);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Stay fullscreen: an accidental gesture can drop us out, and there is no chrome to get back.
        if ui.input(|i| i.viewport().fullscreen) == Some(false) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
        }
        if self.viewer.is_some() {
            self.view(ui);
        } else {
            self.library(ui);
        }
    }
}
