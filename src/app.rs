use crate::cache::ScanCache;
use crate::chart::{Chart, ChartEvent};
use crate::scan::{self, Scan, Volume};
use crate::snapshots;
use egui::{
    self, Align, Align2, Button, Color32, Context, FontId, Frame, Id, Key, Layout, Modal, Pos2,
    Rect, RichText, ScrollArea, Sense, Stroke, Ui, UiBuilder, Vec2, pos2, vec2,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
};
use std::time::{Duration, Instant};

const BG: Color32 = Color32::from_rgb(35, 46, 70);
const PANEL: Color32 = Color32::from_rgb(43, 56, 80);
const LINE: Color32 = Color32::from_rgb(63, 77, 101);
const TEXT: Color32 = Color32::from_rgb(231, 238, 248);
const MUTED: Color32 = Color32::from_rgb(158, 177, 201);
const BLUE: Color32 = Color32::from_rgb(117, 182, 255);
const MINT: Color32 = Color32::from_rgb(102, 230, 190);

pub fn size(bytes: u64) -> String {
    let mut number = bytes as f64;
    let mut unit = 0;
    let units = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    while number >= 1000. && unit + 1 < units.len() {
        number /= 1000.;
        unit += 1;
    }
    if unit == 0 || number >= 100. {
        format!("{number:.0} {}", units[unit])
    } else {
        format!(
            "{} {}",
            format!("{number:.1}").trim_end_matches(".0"),
            units[unit]
        )
    }
}

struct Opened {
    scan: Arc<Scan>,
    focus: usize,
    cached: bool,
    save_epoch: Option<u64>,
}
#[derive(Clone, Copy)]
enum Phase {
    Opening,
    Scanning,
}
enum Update {
    Progress(u64, PathBuf),
    Done(std::io::Result<Opened>),
}

#[derive(Default)]
struct Memory {
    scans: ScanCache,
    generation: Option<u64>,
}
impl Memory {
    fn synchronize(&mut self, generation: Option<u64>) {
        if self.generation != generation {
            self.scans = ScanCache::default();
            self.generation = generation;
        }
    }

    fn invalidate(&mut self, path: &Path) {
        if let Some((before, after)) = snapshots::invalidate(path) {
            self.synchronize(Some(before));
            self.generation = Some(after);
        }
        self.scans.invalidate(path);
    }
}

// Cache validation, filesystem traversal, and serialization all run on the worker.
fn open_scan(
    root: &Path,
    fresh: bool,
    cancel: &AtomicBool,
    memory: &Mutex<Memory>,
    notify: impl Fn(Update),
) -> std::io::Result<Opened> {
    // Explicit invalidation also covers a location which disappeared since scanning.
    let canonical = root.canonicalize();
    if fresh {
        memory
            .lock()
            .unwrap()
            .invalidate(canonical.as_deref().unwrap_or(root));
    }
    let root = canonical?;
    let (generation, remembered) = {
        let mut memory = memory.lock().unwrap();
        let generation = snapshots::generation();
        memory.synchronize(generation);
        let hit = if fresh { None } else { memory.scans.get(&root) };
        (generation, hit)
    };
    let opened = if let Some((scan, focus)) = remembered {
        return Ok(Opened {
            scan,
            focus,
            cached: true,
            save_epoch: None,
        });
    } else if let Some((scan, focus)) = (!fresh).then(|| snapshots::load(&root)).flatten() {
        Opened {
            scan,
            focus,
            cached: true,
            save_epoch: None,
        }
    } else {
        let scan = Arc::new(scan::scan(&root, cancel, |n, p| {
            notify(Update::Progress(n, p.into()))
        })?);
        Opened {
            scan,
            focus: 0,
            cached: false,
            save_epoch: generation,
        }
    };
    // An invalidation in another worker can finish while this snapshot is being read.
    let mut memory = memory.lock().unwrap();
    if !cancel.load(Ordering::Relaxed)
        && generation == snapshots::generation()
        && memory.generation == generation
    {
        memory.scans.insert(opened.scan.clone());
    }
    Ok(opened)
}
struct Overview {
    volumes: Vec<Volume>,
    reusable: HashSet<PathBuf>,
}
struct Job {
    events: Receiver<Update>,
    phase: Phase,
    cancel: Arc<AtomicBool>,
    at: PathBuf,
    count: u64,
    started: Instant,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
#[derive(Clone)]
struct Item {
    target: crate::cleanup::Target,
    bytes: u64,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Dialog {
    Help,
    Location,
    Collector,
    Confirm(bool),
    Details(usize),
    Error,
}

pub struct App {
    volumes: Vec<Volume>,
    pinned: Vec<PathBuf>,
    mounts: Option<Receiver<std::io::Result<Overview>>>,
    memory: Arc<Mutex<Memory>>,
    reusable: HashSet<PathBuf>,
    writers: Vec<std::thread::JoinHandle<()>>,
    view_root: usize,
    cached_view: bool,
    scan: Option<Arc<Scan>>,
    job: Option<Job>,
    explorer: bool,
    chart: Chart,
    focus: usize,
    history: Vec<usize>,
    forward: Vec<usize>,
    selected: Option<usize>,
    map_hover: Option<usize>,
    list_hover: Option<usize>,
    drag: Option<usize>,
    collected: Vec<Item>,
    trash_job: Option<Receiver<Vec<(PathBuf, String)>>>,
    picker: Option<Receiver<Option<PathBuf>>>,
    dialog: Option<Dialog>,
    location: String,
    focus_location: bool,
    preview: crate::preview::Preview,
    error: String,
    accent: Color32,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        Self::initialize(&cc.egui_ctx, initial)
    }
    fn initialize(ctx: &Context, initial: Option<PathBuf>) -> Self {
        let mut style = (*ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.override_text_color = Some(TEXT);
        style.visuals.panel_fill = BG;
        style.visuals.window_fill = PANEL;
        style.visuals.extreme_bg_color = Color32::from_rgb(25, 36, 56);
        style.visuals.widgets.inactive.bg_fill = LINE;
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(78, 98, 128);
        style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1_f32, LINE);
        style.spacing.button_padding = vec2(11., 5.);
        style.spacing.item_spacing = vec2(7., 5.);
        style.animation_time = 0.20;
        ctx.set_style(style);
        let mut app = Self {
            volumes: vec![],
            pinned: crate::favorites::load(),
            mounts: None,
            memory: Arc::default(),
            reusable: HashSet::new(),
            writers: Vec::new(),
            view_root: 0,
            cached_view: false,
            scan: None,
            job: None,
            explorer: false,
            chart: Chart::default(),
            focus: 0,
            history: vec![],
            forward: vec![],
            selected: None,
            map_hover: None,
            list_hover: None,
            drag: None,
            collected: vec![],
            trash_job: None,
            picker: None,
            dialog: None,
            location: String::new(),
            focus_location: false,
            preview: Default::default(),
            error: String::new(),
            accent: crate::theme::accent(),
        };
        app.refresh(ctx);
        if let Some(path) = initial {
            app.start(ctx, path);
        }
        app
    }
    fn refresh(&mut self, ctx: &Context) {
        if self.mounts.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        self.mounts = Some(rx);
        let pinned = self.pinned.clone();
        let memory = self.memory.clone();
        std::thread::spawn(move || {
            let result = scan::volumes().map(|volumes| {
                let roots = snapshots::roots();
                let mut candidates = pinned;
                candidates.extend(std::env::var_os("HOME").map(PathBuf::from));
                candidates.extend(volumes.iter().map(|v| v.path.clone()));
                memory.lock().unwrap().synchronize(snapshots::generation());
                let reusable = candidates
                    .into_iter()
                    .filter(|path| {
                        let Ok(canonical) = path.canonicalize() else {
                            return false;
                        };
                        memory.lock().unwrap().scans.contains(&canonical)
                            || roots.iter().any(|r| scan::can_cover(r, &canonical))
                    })
                    .collect();
                Overview { volumes, reusable }
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    fn start(&mut self, ctx: &Context, root: PathBuf) {
        self.launch(ctx, root, false);
    }

    fn rescan(&mut self, ctx: &Context) {
        if let Some(scan) = self.scan.take() {
            self.launch(ctx, scan.path(self.view_root), true);
        }
    }

    fn launch(&mut self, ctx: &Context, root: PathBuf, fresh: bool) {
        if self.trash_job.is_some() {
            return;
        }
        self.drag = None;
        self.dialog = None;
        self.explorer = false;
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.job = Some(Job {
            phase: Phase::Opening,
            at: root.clone(),
            count: 0,
            cancel: cancel.clone(),
            events: rx,
            started: Instant::now(),
        });
        let ctx = ctx.clone();
        let memory = self.memory.clone();
        std::thread::spawn(move || {
            let result = open_scan(&root, fresh, &cancel, &memory, |update| {
                let _ = tx.send(update);
                ctx.request_repaint();
            });
            let _ = tx.send(Update::Done(result));
            ctx.request_repaint();
        });
    }
    fn fail(&mut self, error: impl Into<String>) {
        self.error = error.into();
        self.dialog = Some(Dialog::Error);
    }
    fn poll(&mut self, ctx: &Context) {
        if let Some(result) = self.mounts.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.mounts = None;
            match result {
                Ok(overview) => {
                    self.volumes = overview.volumes;
                    self.reusable = overview.reusable;
                }
                Err(e) => self.fail(format!("Could not list disks. {e}")),
            }
        }
        if let Some(path) = self.picker.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.picker = None;
            if let Some(path) = path {
                self.start(ctx, path)
            }
        }
        if let Some(job) = &mut self.job {
            let mut result = None;
            for msg in job.events.try_iter() {
                match msg {
                    Update::Progress(n, path) => {
                        job.phase = Phase::Scanning;
                        job.count = n;
                        job.at = path;
                    }
                    Update::Done(scan) => result = Some(scan),
                }
            }
            if let Some(result) = result {
                self.job = None;
                match result {
                    Ok(opened) => {
                        if let Some(epoch) = opened.save_epoch {
                            let new_scan = opened.scan.clone();
                            self.writers.retain(|writer| !writer.is_finished());
                            self.writers.push(std::thread::spawn(move || {
                                snapshots::save_if_current(&new_scan, Some(epoch));
                            }));
                        }
                        self.scan = Some(opened.scan);
                        self.view_root = opened.focus;
                        self.focus = opened.focus;
                        self.cached_view = opened.cached;
                        self.preview = Default::default();
                        self.explorer = true;
                        self.history.clear();
                        self.forward.clear();
                        self.clear_hover();
                        self.chart = Chart::default();
                        self.refresh(ctx);
                    }
                    Err(e) => self.fail(format!("Could not scan this folder. {e}")),
                }
            }
        }
        if let Some(failed) = self.trash_job.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.trash_job = None;
            let affected = self.scan.as_ref().is_some_and(|scan| {
                let view = scan.path(self.view_root);
                self.collected.iter().any(|item| {
                    view.starts_with(&item.target.path) || item.target.path.starts_with(&view)
                })
            });
            self.collected
                .retain(|entry| failed.iter().any(|(p, _)| *p == entry.target.path));
            if affected {
                self.rescan(ctx);
            } else {
                self.refresh(ctx);
            }
            if !failed.is_empty() {
                self.fail(
                    failed
                        .into_iter()
                        .map(|(p, e)| format!("{}\n{e}", p.display()))
                        .collect::<Vec<String>>()
                        .join("\n\n"),
                );
            }
        }
    }
    fn clear_hover(&mut self) {
        self.selected = None;
        self.map_hover = None;
        self.list_hover = None;
        self.drag = None;
    }
    fn open(&mut self, id: usize) {
        let Some(scan) = &self.scan else {
            return;
        };
        if !scan.nodes[id].is_dir {
            self.dialog = Some(Dialog::Details(id));
            return;
        }
        if id != self.focus {
            self.history.push(self.focus);
            self.forward.clear();
            self.focus = id;
            self.clear_hover();
        }
    }
    fn back(&mut self) {
        if let Some(id) = self.history.pop() {
            self.forward.push(self.focus);
            self.focus = id;
            self.clear_hover();
        } else {
            self.explorer = false;
            self.clear_hover();
        }
    }
    fn forward(&mut self) {
        if let Some(id) = self.forward.pop() {
            self.history.push(self.focus);
            self.focus = id;
            self.clear_hover();
        }
    }
    fn up(&mut self) {
        if self.focus != self.view_root
            && let Some(id) = self.scan.as_ref().and_then(|s| s.nodes[self.focus].parent)
        {
            self.open(id);
        } else {
            self.explorer = false;
            self.clear_hover();
        }
    }
    fn collect(&mut self, id: usize) {
        let Some(scan) = &self.scan else {
            return;
        };
        let path = scan.path(id);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if id == self.view_root
            || home.as_deref().is_some_and(|h| h.starts_with(&path))
            || self.volumes.iter().any(|v| v.path.starts_with(&path))
        {
            self.fail("This location is protected. Collect individual items inside it.");
            return;
        }
        if self
            .collected
            .iter()
            .any(|item| path.starts_with(&item.target.path))
        {
            return;
        }
        match crate::cleanup::Target::from_scan(&path, scan.nodes[id].identity) {
            Ok(target) => {
                self.collected
                    .retain(|item| !item.target.path.starts_with(&path));
                self.collected.push(Item {
                    target,
                    bytes: scan.nodes[id].bytes,
                });
            }
            Err(message) => self.fail(message),
        }
    }
    fn choose_folder(&mut self, ctx: &Context) {
        if self.picker.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        self.picker = Some(rx);
        std::thread::spawn(move || {
            let path = rfd::FileDialog::new()
                .set_title("Scan a folder")
                .pick_folder();
            let _ = tx.send(path);
            ctx.request_repaint();
        });
    }
    fn built_in(&self, path: &Path) -> bool {
        std::env::var_os("HOME").as_deref().map(Path::new) == Some(path)
            || self.volumes.iter().any(|v| v.path == path)
    }

    fn toggle_pin(&mut self, path: &Path) {
        if self.built_in(path) {
            return;
        }
        let pin = !self.pinned.iter().any(|p| p == path);
        match crate::favorites::set(path, pin) {
            Ok(updated) => self.pinned = updated,
            Err(error) => self.fail(format!("Could not update pinned folders. {error}")),
        }
    }

    fn overview(&mut self, ui: &mut Ui) {
        ui.add_space(12.);
        ui.horizontal(|ui| {
            ui.heading(RichText::new("Disks and folders").size(27.).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("?").on_hover_text("Keyboard shortcuts").clicked() {
                    self.dialog = Some(Dialog::Help)
                }
                if ui.button("Refresh").clicked() {
                    self.pinned = crate::favorites::load();
                    self.refresh(ui.ctx())
                }
            });
        });
        ui.add_space(18.);
        let bottom = 55.;
        let height = (ui.available_height() - bottom).max(80.);
        let mut target = None;
        let mut unpin = None;
        ScrollArea::vertical()
            .id_salt("volumes")
            .max_height(height)
            .show(ui, |ui| {
                ui.set_min_height(height - 1.);
                if self.volumes.is_empty() && self.mounts.is_some() {
                    ui.label("Finding disks…");
                }
                for volume in &self.volumes {
                    if volume_row(
                        ui,
                        &volume.name,
                        &volume.path,
                        Some(volume),
                        self.accent,
                        self.reusable.contains(&volume.path),
                    )
                    .clicked()
                    {
                        target = Some(volume.path.clone());
                    }
                }
                if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
                    && !self.volumes.iter().any(|volume| volume.path == home)
                {
                    ui.add_space(12.);
                    if volume_row(
                        ui,
                        "Home folder",
                        &home,
                        None,
                        self.accent,
                        self.reusable.contains(&home),
                    )
                    .clicked()
                    {
                        target = Some(home);
                    }
                }
                for path in self.pinned.iter().filter(|path| !self.built_in(path)) {
                    let name = path
                        .file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy();
                    let row = volume_row(
                        ui,
                        &name,
                        path,
                        None,
                        self.accent,
                        self.reusable.contains(path),
                    );
                    let rect = Rect::from_center_size(
                        row.rect.left_center() + vec2(32., 0.),
                        vec2(41., 41.),
                    );
                    let star = ui
                        .interact(rect, Id::new(("unpin", path)), Sense::click())
                        .on_hover_text("Unpin folder");
                    ui.painter().text(
                        rect.center() + vec2(0., 1.),
                        Align2::CENTER_CENTER,
                        "★",
                        FontId::proportional(15.),
                        BG,
                    );
                    if star.clicked() {
                        unpin = Some(path.clone());
                    } else if row.clicked() {
                        target = Some(path.clone());
                    }
                }
            });
        ui.add_space(7.);
        ui.horizontal(|ui| {
            if ui.button("Scan folder…").clicked() {
                self.choose_folder(ui.ctx())
            }
            if ui.button("Enter path…").clicked() {
                self.focus_location = true;
                self.dialog = Some(Dialog::Location)
            }
            if self.scan.is_some() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Return to map  →").clicked() {
                        self.explorer = true;
                    }
                });
            }
        });
        if let Some(path) = unpin {
            self.toggle_pin(&path);
        }
        if let Some(path) = target {
            self.start(ui.ctx(), path)
        }
    }
    fn scanning(&mut self, ui: &mut Ui) {
        let job = self.job.as_ref().unwrap();
        let rect = ui.available_rect_before_wrap();
        let center = rect.center() - vec2(0., 63.);
        let time = job.started.elapsed().as_secs_f32();
        for ring in 0..4 {
            let radius = 25. + ring as f32 * 10.;
            let start = time * (1. - ring as f32 * 0.13) + ring as f32 * 2.;
            let length = 2.3 + ring as f32 * 0.46;
            let points = (0..=70)
                .map(|i| center + Vec2::angled(start + length * i as f32 / 70.) * radius)
                .collect::<Vec<_>>();
            ui.painter()
                .circle_stroke(center, radius, Stroke::new(7_f32, PANEL));
            ui.painter().add(egui::Shape::line(
                points,
                Stroke::new(
                    7_f32,
                    [MINT, BLUE, Color32::from_rgb(170, 155, 252), self.accent][ring],
                ),
            ));
        }
        label_at(
            ui,
            center + vec2(0., 96.),
            Align2::CENTER_CENTER,
            match job.phase {
                Phase::Opening => "Opening map",
                Phase::Scanning => "Scanning",
            },
            26.,
            TEXT,
        );
        label_at(
            ui,
            center + vec2(0., 127.),
            Align2::CENTER_CENTER,
            &match job.phase {
                Phase::Opening => String::new(),
                _ => format!("{} entries  ·  {}s", job.count, time as u64),
            },
            15.,
            MUTED,
        );
        let dir = Rect::from_center_size(center + vec2(0., 153.), vec2(rect.width() - 50., 24.));
        ellipsis(
            ui,
            dir,
            &job.at.to_string_lossy(),
            14.,
            MUTED,
            Align::Center,
        );
        if ui
            .put(
                Rect::from_center_size(center + vec2(0., 199.), vec2(92., 30.)),
                Button::new("Cancel"),
            )
            .clicked()
        {
            self.job = None;
        }
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
    fn toolbar(&mut self, ui: &mut Ui, scan: &Scan) {
        let mut ancestors = vec![self.focus];
        while *ancestors.last().unwrap() != self.view_root
            && let Some(parent) = scan.nodes[*ancestors.last().unwrap()].parent
        {
            ancestors.push(parent);
        }
        ancestors.reverse();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.;
            if ui.button("‹").on_hover_text("Back  ·  Alt+Left").clicked() {
                self.back();
            }
            if ui
                .add_enabled(!self.forward.is_empty(), Button::new("›"))
                .on_hover_text("Forward  ·  Alt+Right")
                .clicked()
            {
                self.forward();
            }
            ui.add_space(8.);
            if crumb(ui, "Disks and folders", 125., true).clicked() {
                self.explorer = false;
                self.clear_hover();
                self.refresh(ui.ctx());
            }
            let available = (ui.available_width() - 185.).max(60.);
            let max_count = ((available / 94.) as usize).max(1);
            if ancestors.len() > max_count {
                ui.label("…");
            }
            let visible = ancestors.len().min(max_count);
            let width = (available / visible as f32 - 7.).clamp(45., 145.);
            for id in ancestors
                .into_iter()
                .rev()
                .take(visible)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                let name = scan.nodes[id].name.to_string_lossy();
                if crumb(ui, &name, width, false)
                    .on_hover_text(scan.path(id).display().to_string())
                    .clicked()
                {
                    self.open(id);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("?").on_hover_text("Keyboard shortcuts").clicked() {
                    self.dialog = Some(Dialog::Help);
                }
                if ui
                    .button("Rescan")
                    .on_hover_text("Rescan this location  ·  R")
                    .clicked()
                {
                    self.rescan(ui.ctx());
                }
                let folder = scan.path(self.focus);
                if !self.built_in(&folder) {
                    let pinned = self.pinned.contains(&folder);
                    let text = if pinned {
                        RichText::new("★ Pinned").color(self.accent)
                    } else {
                        RichText::new("☆ Pin")
                    };
                    if ui
                        .button(text)
                        .on_hover_text("Pin or unpin this folder  ·  Ctrl+D")
                        .clicked()
                    {
                        self.toggle_pin(&folder);
                    }
                }
            });
        });
    }
    fn inspector(&mut self, ui: &mut Ui, scan: &Scan, focus: usize) {
        let node = &scan.nodes[focus];
        let color = if focus == 0 {
            MINT
        } else {
            self.chart.branch_color(scan, focus)
        };
        let (header, _) = ui.allocate_exact_size(vec2(ui.available_width(), 39.), Sense::hover());
        let p = header.left_center() + vec2(6., 0.);
        ui.painter().circle_filled(p, 6., color);
        let amount = label_at(
            ui,
            header.right_center(),
            Align2::RIGHT_CENTER,
            &size(node.bytes),
            18.,
            TEXT,
        );
        ellipsis(
            ui,
            Rect::from_min_max(
                header.min + vec2(22., 0.),
                pos2(amount.left() - 6., header.bottom()),
            ),
            &node.name.to_string_lossy(),
            18.,
            TEXT,
            Align::Min,
        );
        ui.add_space(4.);
        self.list_hover = None;
        if !node.is_dir {
            ui.label(RichText::new("File").color(MUTED));
            ui.add(
                egui::Label::new(RichText::new(scan.path(focus).to_string_lossy()).color(MUTED))
                    .wrap(),
            );
            return;
        }
        if node.children.is_empty() {
            ui.label(RichText::new("No entries to display").color(MUTED));
        }
        ui.spacing_mut().item_spacing.y = 0.;
        let row_height = 26.;
        let volume = (focus == 0)
            .then(|| self.volumes.iter().find(|v| v.path == scan.root))
            .flatten();
        let unmeasured = volume.map_or(0, |v| {
            v.total.saturating_sub(v.free).saturating_sub(node.bytes)
        });
        let free = volume.map(|v| v.free);
        let count =
            node.children.len() + usize::from(volume.is_some()) + usize::from(unmeasured > 0);
        let mut scroll = ScrollArea::vertical()
            .id_salt(("objects", focus))
            .auto_shrink([false, true]);
        if let Some(id) = self.selected
            && ui.input(|i| i.key_pressed(Key::ArrowDown) || i.key_pressed(Key::ArrowUp))
            && let Some(index) = node.children.iter().position(|c| *c == id)
        {
            let area_height = ui.available_height();
            let offset = (index as f32 * row_height - area_height / 2.).max(0.);
            scroll = scroll.vertical_scroll_offset(offset);
        }
        scroll.show_rows(ui, row_height, count, |ui, rows| {
            for i in rows {
                if i >= node.children.len() {
                    let (name, value, dot) = if i == node.children.len() && unmeasured > 0 {
                        ("Unmapped space", unmeasured, crate::chart::UNMAPPED)
                    } else {
                        ("Free space", free.unwrap(), MUTED)
                    };
                    let (rect, r) = ui.allocate_exact_size(
                        vec2(ui.available_width(), row_height),
                        Sense::hover(),
                    );
                    item_line(ui, rect, name, &size(value), dot, MUTED);
                    if name == "Unmapped space" {
                        r.on_hover_text(concat!(
                            "Used space outside the measured files, including ",
                            "filesystem metadata and unreadable entries."
                        ));
                    }
                    continue;
                }
                let id = node.children[i];
                let child = &scan.nodes[id];
                let (rect, response) = ui.allocate_exact_size(
                    vec2(ui.available_width(), row_height),
                    Sense::click_and_drag(),
                );
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                if response.hovered() || self.selected == Some(id) {
                    ui.painter().rect_filled(rect, 4., PANEL);
                }
                item_line(
                    ui,
                    rect,
                    &child.name.to_string_lossy(),
                    &size(child.bytes),
                    self.chart.branch_color(scan, id),
                    TEXT,
                );
                if response.hovered() {
                    self.list_hover = Some(id);
                }
                if response.clicked() {
                    self.open(id);
                }
                if response.drag_started() {
                    self.drag = Some(id);
                }
                response.context_menu(|ui| {
                    if ui.button("File details").clicked() {
                        self.dialog = Some(Dialog::Details(id));
                        ui.close();
                    }
                    if ui.button("Collect").clicked() {
                        self.collect(id);
                        ui.close();
                    }
                });
            }
        });
    }
    fn explorer(&mut self, ui: &mut Ui) {
        let scan = self.scan.as_ref().unwrap().clone();
        self.toolbar(ui, &scan);
        if self.job.is_some() || !self.explorer {
            return;
        }
        ui.add_space(11.);
        let rect = ui.available_rect_before_wrap();
        let split = rect.right() - (rect.width() * 0.32).clamp(195., 400.);
        let map = Rect::from_min_max(rect.min, pos2(split - 10., rect.bottom() - 64.));
        let list = Rect::from_min_max(
            pos2(split + 6., rect.top() + 3.),
            pos2(rect.right(), rect.bottom() - 40.),
        );
        let mut plot = ui.new_child(UiBuilder::new().max_rect(map));
        let volume = if self.focus == 0 {
            self.volumes.iter().find(|v| v.path == scan.root)
        } else {
            None
        };
        self.chart.view_root = self.view_root;
        let out = self.chart.show(
            &mut plot,
            &scan,
            self.focus,
            self.selected.or(self.list_hover),
            volume.map(|v| (v.total, v.free)),
            map.size(),
        );
        self.map_hover = out.hovered;
        if let Some(event) = out.event {
            match event {
                ChartEvent::Open(id) => self.open(id),
                ChartEvent::Back => self.up(),
                ChartEvent::Drag(id) => self.drag = Some(id),
                ChartEvent::Collect(id) => self.collect(id),
            }
        }
        let mut sidebar = ui.new_child(UiBuilder::new().max_rect(list));
        let inspecting = if self.selected.is_some() {
            self.focus
        } else {
            self.map_hover.unwrap_or(self.focus)
        };
        self.inspector(&mut sidebar, &scan, inspecting);
        let foot = Rect::from_min_max(
            pos2(rect.left(), rect.bottom() - 54.),
            pos2(split - 10., rect.bottom()),
        );
        self.collector_bar(ui, foot);
        if self.cached_view || scan.skipped > 0 {
            let mut status = Vec::new();
            if self.cached_view {
                status.push("Cached scan".to_string());
            }
            if scan.skipped > 0 {
                status.push(format!("{} unreadable entries", scan.skipped));
            }
            let msg = status.join("  ·  ");
            let region = Rect::from_min_max(pos2(split, rect.bottom() - 29.), rect.max);
            ellipsis(ui, region, &msg, 12., MUTED, Align::Max);
        }
        if let Some(id) = self.drag
            && let Some(pos) = ui.ctx().pointer_interact_pos()
        {
            if ui.input(|i| i.pointer.any_released() || !i.pointer.any_down()) {
                self.drag = None;
                if foot.contains(pos) {
                    self.collect(id);
                }
            } else {
                let name = scan.nodes[id].name.to_string_lossy();
                let painter = ui.ctx().layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    Id::new("lifted item"),
                ));
                let galley =
                    painter.layout_no_wrap(name.into_owned(), FontId::proportional(14.), TEXT);
                let dims = galley.size().min(vec2(260., 24.)) + vec2(20., 12.);
                let rect = Rect::from_min_size(pos + vec2(13., 12.), dims);
                painter.rect_filled(rect, 12., LINE);
                painter.with_clip_rect(rect.shrink(3.)).galley(
                    rect.min + vec2(10., 6.),
                    galley,
                    TEXT,
                );
            }
        } else if ui.input(|i| !i.pointer.any_down()) {
            self.drag = None;
        }
    }
    fn collector_bar(&mut self, ui: &mut Ui, rect: Rect) {
        let r = ui.interact(rect, Id::new("collector bar"), Sense::click());
        let hot = self.drag.is_some()
            && ui
                .ctx()
                .pointer_interact_pos()
                .is_some_and(|p| rect.contains(p));
        let active = ui
            .ctx()
            .animate_bool(Id::new("collector glow"), hot || r.hovered());
        let center = rect.left_center() + vec2(25., 0.);
        let radius = 22. + 2. * active;
        let occupied = !self.collected.is_empty();
        if occupied || hot {
            let track =
                Rect::from_min_max(center - vec2(0., 15.), pos2(rect.right(), center.y + 15.));
            ui.painter().rect_filled(track, 14., PANEL);
        }
        let edge = if hot {
            MINT
        } else if occupied {
            BLUE
        } else {
            LINE
        };
        ui.painter().circle_filled(center, radius, BG);
        ui.painter()
            .circle_stroke(center, radius, Stroke::new(2_f32, edge));
        ui.painter()
            .circle_stroke(center, radius - 6., Stroke::new(2_f32, edge));
        if occupied {
            label_at(
                ui,
                center,
                Align2::CENTER_CENTER,
                &self.collected.len().to_string(),
                14.,
                TEXT,
            );
        } else {
            ui.painter().circle_filled(center, radius - 12., edge);
        }
        let total = self
            .collected
            .iter()
            .fold(0u64, |s, t| s.saturating_add(t.bytes));
        let msg = if self.trash_job.is_some() {
            "Removing items…".to_owned()
        } else if hot {
            "Release to collect".into()
        } else if occupied {
            format!("{} collected", size(total))
        } else {
            "Drag files here to collect them".into()
        };
        label_at(
            ui,
            center + vec2(37., 0.),
            Align2::LEFT_CENTER,
            &msg,
            15.,
            if occupied { TEXT } else { MUTED },
        );
        if occupied {
            label_at(
                ui,
                pos2(rect.right() - 12., center.y),
                Align2::RIGHT_CENTER,
                "Review  ›",
                14.,
                BLUE,
            );
        }
        if r.clicked() && self.trash_job.is_none() {
            self.dialog = Some(Dialog::Collector);
        }
    }
    fn shortcuts(&mut self, ctx: &Context) {
        if self.dialog.is_some()
            || self.trash_job.is_some()
            || self.picker.is_some()
            || egui::Popup::is_any_open(ctx)
        {
            return;
        }
        let key = |k| ctx.input(|i| i.key_pressed(k));
        let modifiers_at_press = |target| {
            ctx.input(|input| {
                input
                    .events
                    .iter()
                    .find_map(|event| match event {
                        egui::Event::Key {
                            key,
                            modifiers,
                            pressed: true,
                            ..
                        } if *key == target => Some(*modifiers),
                        _ => None,
                    })
                    .unwrap_or_default()
            })
        };
        if key(Key::Questionmark) || (modifiers_at_press(Key::Slash).shift) {
            self.dialog = Some(Dialog::Help);
            return;
        }
        if key(Key::Q) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if self.job.is_some() {
            if key(Key::Escape) {
                self.job = None;
            }
            return;
        }
        if modifiers_at_press(Key::O).command {
            self.choose_folder(ctx);
            return;
        }
        if modifiers_at_press(Key::L).command {
            self.dialog = Some(Dialog::Location);
            self.focus_location = true;
            return;
        }
        if !self.explorer {
            if key(Key::R) {
                self.refresh(ctx);
            }
            if key(Key::Space)
                && let Some(v) = self.volumes.first()
            {
                self.start(ctx, v.path.clone());
            }
            return;
        }
        let scan = self.scan.as_ref().unwrap().clone();
        if modifiers_at_press(Key::D).command {
            self.toggle_pin(&scan.path(self.focus));
            return;
        }
        let children = &scan.nodes[self.focus].children;
        if (modifiers_at_press(Key::ArrowLeft).alt) || key(Key::Backspace) {
            self.back();
            return;
        }
        if modifiers_at_press(Key::ArrowRight).alt {
            self.forward();
            return;
        }
        if key(Key::Escape) || key(Key::ArrowLeft) {
            self.up();
            return;
        }
        let target = self.selected.or(self.list_hover).or(self.map_hover);
        if key(Key::Space)
            && let Some(id) = target
        {
            self.dialog = Some(Dialog::Details(id));
            return;
        }
        if key(Key::Delete)
            && let Some(id) = target
        {
            self.collect(id);
            return;
        }
        if key(Key::C) {
            self.dialog = Some(Dialog::Collector);
            return;
        }
        if key(Key::R) {
            self.rescan(ctx);
            return;
        }
        if (key(Key::Enter) || key(Key::ArrowRight))
            && let Some(id) = self.selected
        {
            self.open(id);
            return;
        }
        if !children.is_empty() && (key(Key::ArrowUp) || key(Key::ArrowDown)) {
            let current = self
                .selected
                .and_then(|id| children.iter().position(|c| *c == id));
            let idx = if key(Key::ArrowDown) {
                current.map_or(0, |i| (i + 1).min(children.len() - 1))
            } else {
                current.map_or(children.len() - 1, |i| i.saturating_sub(1))
            };
            self.selected = Some(children[idx]);
            self.map_hover = None;
        }
    }
    fn help_dialog(&mut self, ui: &mut Ui) {
        ui.heading("Keyboard shortcuts");
        ui.add_space(8.);
        egui::Grid::new("shortcut list")
            .spacing(vec2(28., 8.))
            .show(ui, |ui| {
                for (key, action) in [
                    ("Ctrl + O", "Choose a folder"),
                    ("Ctrl + L", "Enter a path"),
                    ("Ctrl + D", "Pin or unpin this folder"),
                    ("↑   ↓", "Select an item"),
                    ("Enter   →", "Explore selection"),
                    ("Esc   ←", "Go to parent or cancel scan"),
                    ("Alt + ←   Alt + →", "Back and forward"),
                    ("Space", "Inspect item or scan first disk"),
                    ("Delete", "Add to collector"),
                    ("C", "Review collector"),
                    ("R", "Rescan or refresh disks"),
                    ("?", "Keyboard shortcuts"),
                    ("Q", "Quit"),
                ] {
                    ui.label(RichText::new(key).color(BLUE));
                    ui.label(action);
                    ui.end_row();
                }
            });
        ui.add_space(10.);
        if ui.button("Done").clicked() {
            self.dialog = None;
        }
    }
    fn location_dialog(&mut self, ctx: &Context, ui: &mut Ui) {
        ui.heading("Scan a folder");
        ui.add_space(6.);
        let field = ui.add(
            egui::TextEdit::singleline(&mut self.location)
                .hint_text("/path/to/folder")
                .desired_width(f32::INFINITY),
        );
        if self.focus_location {
            field.request_focus();
            self.focus_location = false;
        }
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                self.dialog = None;
            }
            if (ui
                .add_enabled(!self.location.is_empty(), Button::new("Scan"))
                .clicked()
                || enter)
                && !self.location.is_empty()
            {
                let mut path = PathBuf::from(&self.location);
                if let Ok(relative) = path.strip_prefix("~")
                    && let Some(home) = std::env::var_os("HOME")
                {
                    path = PathBuf::from(home).join(relative);
                }
                self.start(ctx, path);
            }
        });
    }
    fn collector_dialog(&mut self, ctx: &Context, ui: &mut Ui) {
        ui.heading("Collector");
        if self.collected.is_empty() {
            ui.add_space(8.);
            ui.label("Drag an item from the map or list into the collector.");
        } else {
            let total = self
                .collected
                .iter()
                .fold(0u64, |n, item| n.saturating_add(item.bytes));
            ui.label(
                RichText::new(format!(
                    "{} in {} selected",
                    size(total),
                    plural(self.collected.len(), "item")
                ))
                .color(MUTED),
            );
            ui.add_space(6.);
            let mut remove = None;
            ScrollArea::vertical()
                .max_height((ctx.screen_rect().height() * 0.42).min(320.))
                .show(ui, |ui| {
                    for (index, entry) in self.collected.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let (rect, r) = ui.allocate_exact_size(
                                vec2((ui.available_width() - 41.).max(100.), 44.),
                                Sense::hover(),
                            );
                            let path = &entry.target.path;
                            let name = path.file_name().unwrap_or_default().to_string_lossy();
                            let mut top = rect;
                            top.max.y = top.top() + 23.;
                            let amount = label_at(
                                ui,
                                top.right_center(),
                                Align2::RIGHT_CENTER,
                                &size(entry.bytes),
                                13.,
                                MUTED,
                            );
                            top.max.x = amount.left() - 5.;
                            ellipsis(ui, top, &name, 15., TEXT, Align::Min);
                            let mut bottom = rect;
                            bottom.min.y += 23.;
                            ellipsis(ui, bottom, &path.to_string_lossy(), 12., MUTED, Align::Min);
                            r.on_hover_text(path.display().to_string());
                            if ui
                                .button("×")
                                .on_hover_text("Remove from collector")
                                .clicked()
                            {
                                remove = Some(index);
                            }
                        });
                        ui.separator();
                    }
                });
            if let Some(index) = remove {
                self.collected.remove(index);
            }
            ui.add_space(9.);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Move to Trash…").clicked() {
                    self.dialog = Some(Dialog::Confirm(false));
                }
                if ui.button("Delete permanently…").clicked() {
                    self.dialog = Some(Dialog::Confirm(true));
                }
            });
        }
        ui.add_space(8.);
        if ui.button("Close").clicked() {
            self.dialog = None;
        }
    }
    fn confirm_dialog(&mut self, ctx: &Context, ui: &mut Ui, permanent: bool) {
        let count = plural(self.collected.len(), "item");
        ui.heading(if permanent {
            "Delete permanently?"
        } else {
            "Move to Trash?"
        });
        ui.add_space(6.);
        let message = if permanent {
            format!("Permanently delete {count}, including folder contents? This cannot be undone.")
        } else {
            format!("Move {count} to the desktop Trash? Empty the Trash to reclaim the space.")
        };
        ui.label(message);
        ui.add_space(10.);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                self.dialog = Some(Dialog::Collector);
            }
            let title = if permanent {
                "Delete permanently"
            } else {
                "Move to Trash"
            };
            if ui
                .add(
                    Button::new(RichText::new(title).color(Color32::WHITE))
                        .fill(Color32::from_rgb(142, 58, 83)),
                )
                .clicked()
            {
                let targets = self
                    .collected
                    .iter()
                    .map(|i| i.target.clone())
                    .collect::<Vec<_>>();
                let (tx, rx) = mpsc::channel();
                self.trash_job = Some(rx);
                let memory = self.memory.clone();
                self.dialog = None;
                let ctx = ctx.clone();
                std::thread::spawn(move || {
                    let failed = if permanent {
                        crate::cleanup::delete_permanently(&targets)
                    } else {
                        crate::cleanup::trash(&targets)
                    };
                    // Even a failed recursive deletion can have removed some descendants.
                    let mut memory = memory.lock().unwrap();
                    for target in &targets {
                        memory.invalidate(&target.path);
                    }
                    let _ = tx.send(failed);
                    ctx.request_repaint();
                });
            }
        });
    }
    fn details_dialog(&mut self, ui: &mut Ui, id: usize) {
        let scan = self.scan.as_ref().unwrap().clone();
        let node = &scan.nodes[id];
        let path = scan.path(id);
        ui.heading(node.name.to_string_lossy());
        ui.label(RichText::new(path.to_string_lossy()).color(MUTED));
        ui.add_space(8.);
        ui.label(format!("{} on disk", size(node.bytes)));
        if node.is_dir {
            ui.label(format!(
                "{} in folder",
                plural(node.children.len(), "entry")
            ));
        }
        self.preview.show(ui, &path);
        ui.add_space(10.);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Show in folder").clicked() {
                let dir = path.parent().unwrap_or(&path);
                if let Err(e) = std::process::Command::new("xdg-open")
                    .arg(dir)
                    .spawn()
                    .map(|mut child| std::thread::spawn(move || child.wait()))
                {
                    self.fail(format!("Could not open the file manager. {e}"));
                }
            }
            if ui
                .add_enabled(id != self.view_root, Button::new("Collect"))
                .clicked()
            {
                self.dialog = None;
                self.collect(id);
            }
            if ui.button("Done").clicked() {
                self.dialog = None;
            }
        });
    }
    fn error_dialog(&mut self, ctx: &Context, ui: &mut Ui) {
        ui.heading("Couldn’t complete the action");
        ScrollArea::vertical()
            .max_height(ctx.screen_rect().height() * 0.45)
            .show(ui, |ui| {
                ui.label(&self.error);
            });
        ui.add_space(8.);
        if ui.button("Close").clicked() {
            self.dialog = None;
        }
    }
    fn dialogs(&mut self, ctx: &Context) {
        let Some(dialog) = self.dialog else {
            return;
        };
        let frame = Frame::new()
            .inner_margin(20)
            .corner_radius(9)
            .fill(PANEL)
            .stroke(Stroke::new(1_f32, LINE));
        let result = Modal::new(Id::new("corolla dialog"))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width((ctx.screen_rect().width() - 80.).clamp(260., 455.));
                match dialog {
                    Dialog::Help => self.help_dialog(ui),
                    Dialog::Location => self.location_dialog(ctx, ui),
                    Dialog::Collector => self.collector_dialog(ctx, ui),
                    Dialog::Confirm(permanent) => self.confirm_dialog(ctx, ui, permanent),
                    Dialog::Details(id) => self.details_dialog(ui, id),
                    Dialog::Error => self.error_dialog(ctx, ui),
                }
            });
        if result.should_close() && self.dialog == Some(dialog) {
            self.dialog = None;
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.job.take();
        // A completed scan is visible immediately. Finish publishing it on normal exit.
        for writer in self.writers.drain(..) {
            let _ = writer.join();
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _: &mut eframe::Frame) {
        if self.trash_job.is_some() && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        self.poll(ctx);
        self.accent = crate::theme::accent();
        ctx.request_repaint_after(Duration::from_secs(1));
        if ctx.input(|i| i.pointer.delta() != Vec2::ZERO) {
            self.selected = None;
        }
        self.shortcuts(ctx);
        if self.dialog.is_none()
            && self.trash_job.is_none()
            && let Some(path) =
                ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()))
        {
            self.start(ctx, path);
        }
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(19, 13)),
            )
            .show(ctx, |ui| {
                ui.add_enabled_ui(self.trash_job.is_none(), |ui| {
                    if self.job.is_some() {
                        self.scanning(ui);
                    } else if self.explorer {
                        self.explorer(ui);
                    } else {
                        self.overview(ui);
                    }
                });
            });
        self.dialogs(ctx);
    }
}

fn plural(count: usize, unit: &str) -> String {
    let suffix = if count == 1 {
        unit.to_owned()
    } else if unit == "entry" {
        "entries".into()
    } else {
        format!("{unit}s")
    };
    format!("{count} {suffix}")
}

fn label_at(ui: &Ui, pos: Pos2, anchor: Align2, text: &str, font: f32, color: Color32) -> Rect {
    ui.painter()
        .text(pos, anchor, text, FontId::proportional(font), color)
}
fn ellipsis(ui: &Ui, rect: Rect, text: &str, font: f32, color: Color32, align: Align) {
    if rect.width() < 3. {
        return;
    }
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.to_owned(),
        FontId::proportional(font),
        color,
    );
    job.wrap.max_width = rect.width();
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    let x = match align {
        Align::Min => rect.left(),
        Align::Center => rect.center().x - galley.size().x / 2.,
        Align::Max => rect.right() - galley.size().x,
    };
    let pos = pos2(x, rect.center().y - galley.size().y / 2.);
    ui.painter()
        .with_clip_rect(rect.intersect(ui.clip_rect()))
        .galley(pos, galley, color);
}
fn item_line(ui: &Ui, rect: Rect, name: &str, value: &str, dot: Color32, color: Color32) {
    ui.painter()
        .circle_filled(rect.left_center() + vec2(6., 0.), 3.8, dot);
    let size = label_at(
        ui,
        rect.right_center() - vec2(2., 0.),
        Align2::RIGHT_CENTER,
        value,
        15.,
        color,
    );
    let name_rect = Rect::from_min_max(
        rect.min + vec2(20., 0.),
        pos2(size.left() - 9., rect.bottom()),
    );
    ellipsis(ui, name_rect, name, 15., color, Align::Min);
}
fn crumb(ui: &mut Ui, label: &str, max_width: f32, root: bool) -> egui::Response {
    let width = (ui
        .painter()
        .layout_no_wrap(label.to_owned(), FontId::proportional(13.), TEXT)
        .size()
        .x
        + 24.)
        .min(max_width);
    let (r, response) = ui.allocate_exact_size(vec2(width, 26.), Sense::click());
    let points = vec![
        r.left_top(),
        r.right_top() - vec2(6., 0.),
        r.right_center(),
        r.right_bottom() - vec2(6., 0.),
        r.left_bottom(),
    ];
    let color = if response.hovered() {
        LINE
    } else if root {
        Color32::from_rgb(53, 92, 143)
    } else {
        PANEL
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
    ellipsis(ui, r.shrink2(vec2(7., 0.)), label, 13., TEXT, Align::Center);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}
fn volume_row(
    ui: &mut Ui,
    name: &str,
    path: &Path,
    volume: Option<&Volume>,
    accent: Color32,
    reusable: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 73.), Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let base = if response.hovered() {
        Color32::from_rgb(49, 65, 91)
    } else {
        PANEL
    };
    ui.painter().rect_filled(rect, 6., base);
    let center = rect.left_center() + vec2(32., 0.);
    drive_icon(ui, center, volume.is_none(), accent);
    let left = rect.left() + 66.;
    let right = rect.right() - 16.;
    let gauge_width = (rect.width() * 0.23).clamp(80., 260.);
    let gauge_x = right - 90. - gauge_width;
    let mut text = Rect::from_min_max(
        pos2(left, center.y - 23.),
        pos2(gauge_x - 12., center.y + 2.),
    );
    ellipsis(ui, text, name, 18., TEXT, Align::Min);
    text.min.y = center.y + 2.;
    text.max.y = center.y + 23.;
    let subtitle = match volume {
        Some(v) => format!("{}  ·  {}  ·  {}", size(v.total), v.fs_type, path.display()),
        None => path.display().to_string(),
    };
    ellipsis(ui, text, &subtitle, 13., MUTED, Align::Min);
    if let Some(v) = volume {
        let used =
            (v.total.saturating_sub(v.free) as f64 / v.total.max(1) as f64).clamp(0., 1.) as f32;
        let gauge = Rect::from_min_size(pos2(gauge_x, center.y - 10.), vec2(gauge_width, 5.));
        ui.painter().rect_filled(gauge, 3., BG);
        let c = if used > 0.90 {
            Color32::from_rgb(252, 121, 144)
        } else if used > 0.65 {
            Color32::from_rgb(247, 194, 102)
        } else {
            MINT
        };
        ui.painter().rect_filled(
            Rect::from_min_size(gauge.min, vec2(gauge_width * used, 5.)),
            3.,
            c,
        );
        label_at(
            ui,
            pos2(gauge.right(), center.y + 10.),
            Align2::RIGHT_CENTER,
            &format!("{} free", size(v.free)),
            13.,
            MUTED,
        );
    }
    let button = Rect::from_center_size(pos2(right - 30., center.y), vec2(63., 27.));
    ui.painter().rect_filled(
        button,
        14.,
        if response.hovered() {
            Color32::from_rgb(91, 118, 155)
        } else {
            LINE
        },
    );
    label_at(
        ui,
        button.center(),
        Align2::CENTER_CENTER,
        if reusable { "Open" } else { "Scan" },
        14.,
        TEXT,
    );
    response
}
fn drive_icon(ui: &Ui, c: Pos2, folder: bool, accent: Color32) {
    let p = ui.painter();
    if folder {
        let body = Rect::from_center_size(c + vec2(0., 2.), vec2(33., 21.));
        p.rect_filled(
            Rect::from_min_size(body.min - vec2(0., 4.), vec2(15., 10.)),
            3.,
            Color32::from_rgb(84, 151, 204),
        );
        p.rect_filled(body, 3., Color32::from_rgb(116, 190, 234));
    } else {
        let top = vec![
            c + vec2(-12., -17.),
            c + vec2(12., -17.),
            c + vec2(17., 8.),
            c + vec2(-17., 8.),
        ];
        p.add(egui::Shape::convex_polygon(
            top,
            Color32::from_rgb(159, 179, 201),
            Stroke::NONE,
        ));
        let front = Rect::from_min_size(c + vec2(-17., 7.), vec2(34., 10.));
        p.rect_filled(front, 2., Color32::from_rgb(95, 117, 143));
        p.line_segment(
            [c + vec2(-10., 12.), c + vec2(3., 12.)],
            Stroke::new(1.5_f32, BG),
        );
        p.circle_filled(c + vec2(10., 12.), 1.8, accent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finish_open(app: &mut App, ctx: &Context) -> Arc<Scan> {
        let deadline = Instant::now() + Duration::from_secs(8);
        while app.job.is_some() && Instant::now() < deadline {
            app.poll(ctx);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(app.job.is_none() && app.explorer, "{}", app.error);
        app.scan.as_ref().unwrap().clone()
    }

    #[test]
    fn reopens_subtrees_refreshes_explicitly_and_restores_on_restart() {
        const ISOLATED: &str = "COROLLA_TEST_CACHE_DIRECTORY";
        let Some(dir) = std::env::var_os(ISOLATED).map(PathBuf::from) else {
            // Isolate the GUI's actual XDG storage without mutating process-wide
            // environment variables while the other tests are running.
            let temp = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "app::tests::reopens_subtrees_refreshes_explicitly_and_restores_on_restart",
                ])
                .env(ISOLATED, temp.path())
                .env("XDG_CACHE_HOME", temp.path().join("cache"))
                .env("XDG_STATE_HOME", temp.path().join("state"))
                .status()
                .unwrap();
            assert!(status.success());
            return;
        };
        let a = dir.join("A");
        let child = a.join("child");
        let b = dir.join("B");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir(&b).unwrap();
        let ctx = Context::default();
        let mut app = App::initialize(&ctx, Some(a.clone()));
        let first = finish_open(&mut app, &ctx);
        app.start(&ctx, b.clone());
        let other = finish_open(&mut app, &ctx);
        std::fs::write(child.join("new file"), "new data").unwrap();

        let (picker, rx) = mpsc::channel();
        app.picker = Some(rx);
        picker.send(Some(child.clone())).unwrap();
        app.poll(&ctx);
        let opened = finish_open(&mut app, &ctx);
        assert!(app.cached_view && Arc::ptr_eq(&opened, &first));
        assert_eq!(opened.path(app.view_root), child);
        assert!(opened.nodes[app.focus].children.is_empty());
        app.up();
        assert!(!app.explorer);
        app.rescan(&ctx);
        let updated = finish_open(&mut app, &ctx);
        assert!(!app.cached_view);
        assert_eq!(updated.root, child);
        assert_eq!(updated.nodes[app.focus].children.len(), 1);

        app.start(&ctx, b.clone());
        assert!(Arc::ptr_eq(&finish_open(&mut app, &ctx), &other));
        drop(app);

        let mut reopened = App::initialize(&ctx, Some(child));
        assert_eq!(finish_open(&mut reopened, &ctx).nodes[0].children.len(), 1);
        assert!(reopened.cached_view);
        reopened.start(&ctx, b);
        finish_open(&mut reopened, &ctx);
        assert!(reopened.cached_view);
        reopened.start(&ctx, a);
        finish_open(&mut reopened, &ctx);
        assert!(!reopened.cached_view);
    }
}
