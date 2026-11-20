use crate::{app::size as format_size, scan::Scan};
use egui::{Align2, Color32, CursorIcon, FontId, Mesh, Pos2, Sense, Stroke, Ui, Vec2};
use std::{
    borrow::Cow,
    collections::HashMap,
    f32::consts::{FRAC_PI_2, TAU},
};

const HUB: f32 = 0.17;
const BANDS: [f32; 9] = [HUB, 0.30, 0.43, 0.56, 0.69, 0.82, 0.915, 0.967, 1.0];
pub const UNMAPPED: Color32 = Color32::from_rgb(149, 94, 182);
const FILE: Color32 = Color32::from_rgb(124, 134, 152);

pub enum ChartEvent {
    Open(usize),
    Back,
    Drag(usize),
    BlockedDrag(&'static str),
    Collect(usize),
}

#[derive(Default)]
pub struct ChartOutput {
    pub hovered: Option<usize>,
    pub event: Option<ChartEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Petal {
    Entry(usize),
    Unmapped,
    // The trailing children of this directory are too small to hit separately.
    Smaller { parent: usize, start: usize },
}

#[derive(Clone, Copy)]
struct Sector {
    start: f32,
    end: f32,
    inner: f32,
    outer: f32,
    color: Color32,
}

impl Sector {
    fn tween(self, to: Self, t: f32) -> Self {
        Self {
            start: lerp(self.start, to.start, t),
            end: lerp(self.end, to.end, t),
            inner: lerp(self.inner, to.inner, t),
            outer: lerp(self.outer, to.outer, t),
            color: tint(self.color, to.color, t),
        }
    }

    fn into_arc(self, arc: Self) -> Self {
        let angle = arc.end - arc.start;
        Self {
            start: arc.start + self.start / TAU * angle,
            end: arc.start + self.end / TAU * angle,
            inner: arc.outer + (1.0 - arc.outer) * self.inner,
            outer: arc.outer + (1.0 - arc.outer) * self.outer,
            ..self
        }
    }

    fn folded(self) -> Self {
        Self {
            start: self.start * 0.16,
            end: self.end * 0.16,
            inner: HUB + (self.inner - HUB) * 0.25,
            outer: HUB + (self.outer - HUB) * 0.25,
            ..self
        }
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn tint(a: Color32, b: Color32, t: f32) -> Color32 {
    let channel = |x: u8, y: u8| lerp(x as f32, y as f32, t).round() as u8;
    Color32::from_rgb(
        channel(a.r(), b.r()),
        channel(a.g(), b.g()),
        channel(a.b(), b.b()),
    )
}

fn petal_color(midpoint: f64, depth: usize) -> Color32 {
    // The angle sets the hue, so a large mint branch naturally splits into
    // golden, green and blue children instead of a stack of identical rings.
    const STOPS: [[u8; 3]; 8] = [
        [255, 153, 118],
        [254, 222, 113],
        [176, 243, 109],
        [65, 232, 155],
        [77, 228, 224],
        [104, 146, 255],
        [198, 108, 248],
        [252, 71, 140],
    ];
    let position = midpoint.clamp(0.0, 1.0) as f32 * (STOPS.len() - 1) as f32;
    let index = (position.floor() as usize).min(STOPS.len() - 2);
    let [ar, ag, ab] = STOPS[index];
    let [br, bg, bb] = STOPS[index + 1];
    let hue = tint(
        Color32::from_rgb(ar, ag, ab),
        Color32::from_rgb(br, bg, bb),
        position - index as f32,
    );
    tint(hue, Color32::WHITE, (depth.min(6) as f32) * 0.035)
}

struct Layout<'a> {
    scan: &'a Scan,
    // Only the held node and the ancestors between it and the hub lose bytes.
    adjusted: HashMap<usize, u64>,
    root_bytes: f64,
    extent: f64,
    start_angle: f64,
    radius: f32,
    entries: Vec<(Petal, Sector)>,
}

impl Layout<'_> {
    fn children(&mut self, parent: usize, mut offset: f64, depth: usize) {
        if depth + 1 >= BANDS.len() {
            return;
        }
        let original = &self.scan.nodes[parent].children;
        // Just one member of a sibling list can shrink. Move that ancestor
        // down to its new rank without sorting potentially millions of files.
        let mut children: Cow<'_, [usize]> = Cow::Borrowed(original);
        if self.adjusted.contains_key(&parent)
            && let Some((rank, amount)) = original
                .iter()
                .enumerate()
                .find_map(|(rank, id)| self.adjusted.get(id).copied().map(|bytes| (rank, bytes)))
            && amount > 0
        {
            let shift =
                original[rank + 1..].partition_point(|&id| self.scan.nodes[id].bytes > amount);
            if shift > 0 {
                children.to_mut()[rank..=rank + shift].rotate_left(1);
            }
        }
        for (index, &id) in children.iter().enumerate() {
            let node = &self.scan.nodes[id];
            let bytes = self.adjusted.get(&id).copied().unwrap_or(node.bytes);
            if bytes == 0 {
                if node.bytes != 0 {
                    continue;
                } // This branch is in the pointer's hand.
                break;
            }
            let span = bytes as f64 / self.root_bytes;
            let inner = BANDS[depth];
            let outer = BANDS[depth + 1];
            let tiny = span * self.extent * f64::from(outer * self.radius) < 2.3;
            let (petal, span, color) = if tiny {
                let bytes = children[index..]
                    .iter()
                    .map(|&i| {
                        self.adjusted
                            .get(&i)
                            .copied()
                            .unwrap_or(self.scan.nodes[i].bytes) as f64
                    })
                    .sum::<f64>();
                let span = bytes / self.root_bytes;
                let color = tint(
                    petal_color(offset + span * 0.5, depth),
                    Color32::from_rgb(41, 52, 74),
                    0.56,
                );
                (
                    Petal::Smaller {
                        parent,
                        start: index,
                    },
                    span,
                    color,
                )
            } else {
                let color = if node.is_dir {
                    petal_color(offset + span * 0.5, depth)
                } else {
                    FILE
                };
                (Petal::Entry(id), span, color)
            };
            let start = self.start_angle + offset * self.extent;
            self.entries.push((
                petal,
                Sector {
                    start: start as f32,
                    end: (start + span * self.extent) as f32,
                    inner,
                    outer,
                    color,
                },
            ));
            if tiny {
                break;
            }
            self.children(id, offset, depth + 1);
            offset += span;
        }
    }
}

#[derive(Clone, Copy)]
struct Motion {
    petal: Petal,
    from: Sector,
    to: Sector,
    from_alpha: f32,
    to_alpha: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Swatch {
    Entry(usize),
    SmallerWithin(usize),
}

#[derive(Default)]
pub struct Chart {
    // A directory reopened from an ancestor snapshot can be a nonzero node.
    pub view_root: usize,
    focus: Option<usize>,
    radius: f32,
    motions: Vec<Motion>,
    volume: Option<(u64, u64)>,
    swatches: HashMap<Swatch, Color32>,
    started: f64,
    press: Option<ChartEvent>,
    held: Option<usize>,
    layout_held: Option<usize>,
    lift_graphic: Option<(usize, Vec<Sector>)>,
    duration: f64,
    displayed_bytes: (u64, u64),
    small: Option<(usize, usize)>,
    hover_state: Option<Petal>,
    previous_hover: Option<Petal>,
    hover_changed: f64,
}

impl Chart {
    pub fn set_dragged(&mut self, id: Option<usize>) {
        self.held = id;
    }

    fn dragged_bytes(&self, scan: &Scan, focus: usize) -> HashMap<usize, u64> {
        let mut adjusted = HashMap::new();
        let Some(mut id) = self.held.filter(|&i| i != focus && i < scan.nodes.len()) else {
            return adjusted;
        };
        let bytes = scan.nodes[id].bytes;
        loop {
            adjusted.insert(id, scan.nodes[id].bytes.saturating_sub(bytes));
            if id == focus {
                return adjusted;
            }
            let Some(parent) = scan.nodes[id].parent else {
                return HashMap::new();
            };
            id = parent;
        }
    }

    // Preserve the branch's visible color and its pattern of outer rings before
    // the main chart closes the gap. Tiny inspector entries use a local layout.
    fn capture_graphic(&self, scan: &Scan, id: usize) -> Vec<Sector> {
        let source = self
            .motions
            .iter()
            .find(|m| m.petal == Petal::Entry(id) && m.to_alpha > 0.0)
            .map(|m| m.to);
        let mut sectors = if let Some(base) = source {
            self.motions
                .iter()
                .filter_map(|m| {
                    let mut child = match m.petal {
                        Petal::Entry(c) => c,
                        Petal::Smaller { parent, .. } => parent,
                        Petal::Unmapped => return None,
                    };
                    let s = m.to;
                    if m.to_alpha == 0.0
                        || s.inner < base.inner
                        || s.end < base.start
                        || s.start > base.end
                    {
                        return None;
                    }
                    while child != id {
                        child = scan.nodes[child].parent?;
                    }
                    Some(s)
                })
                .collect::<Vec<_>>()
        } else {
            let base = Sector {
                start: 0.0,
                end: 1.4,
                inner: HUB,
                outer: BANDS[1],
                color: self.branch_color(scan, id),
            };
            let mut layout = Layout {
                scan,
                adjusted: HashMap::new(),
                root_bytes: scan.nodes[id].bytes.max(1) as f64,
                extent: 1.4,
                start_angle: 0.0,
                radius: 65.0,
                entries: Vec::new(),
            };
            layout.children(id, 0.0, 1);
            let mut sectors = vec![base];
            for (key, mut s) in layout.entries {
                if let Petal::Entry(i) = key
                    && let Some(color) = self.swatches.get(&Swatch::Entry(i))
                {
                    s.color = *color;
                }
                sectors.push(s);
            }
            sectors
        };
        if let Some(base) = source.or_else(|| sectors.first().copied()) {
            let span = (base.end - base.start).max(f32::EPSILON);
            let sweep = span.clamp(1.2, 2.3);
            let thickness = sectors.iter().map(|s| s.outer).fold(base.outer, f32::max) - base.inner;
            for s in &mut sectors {
                s.start = -FRAC_PI_2 + ((s.start - base.start) / span - 0.5) * sweep;
                s.end = -FRAC_PI_2 + ((s.end - base.start) / span - 0.5) * sweep;
                s.inner = 0.18 + 0.82 * (s.inner - base.inner) / thickness;
                s.outer = 0.18 + 0.82 * (s.outer - base.inner) / thickness;
            }
        }
        sectors
    }

    /// Paint the detached branch in a 42-point square centered on the drag badge.
    pub fn paint_drag(&mut self, painter: &egui::Painter, scan: &Scan, id: usize, center: Pos2) {
        if id >= scan.nodes.len() {
            return;
        }
        if self.lift_graphic.as_ref().map(|(id, _)| *id) != Some(id) {
            self.lift_graphic = Some((id, self.capture_graphic(scan, id)));
        }
        if let Some((_, sectors)) = &self.lift_graphic {
            let mut mesh = Mesh::default();
            for &s in sectors {
                draw_sector(&mut mesh, s, s.color, center + Vec2::new(0.0, 12.0), 22.0);
            }
            painter.add(mesh);
        }
    }

    pub fn branch_color(&self, scan: &Scan, id: usize) -> Color32 {
        if !scan.nodes[id].is_dir {
            return FILE;
        }
        if self.held == Some(id)
            && let Some((lifted, sectors)) = &self.lift_graphic
            && *lifted == id
            && let Some(root) = sectors.first()
        {
            return root.color;
        }
        self.swatches
            .get(&Swatch::Entry(id))
            .or_else(|| {
                scan.nodes[id]
                    .parent
                    .and_then(|p| self.swatches.get(&Swatch::SmallerWithin(p)))
            })
            .copied()
            .unwrap_or(FILE)
    }

    fn update_layout(
        &mut self,
        scan: &Scan,
        focus: usize,
        volume: Option<(u64, u64)>,
        radius: f32,
        now: f64,
    ) {
        let nav = self.focus != Some(focus);
        let capacity_changed = self.volume != volume;
        let lift_changed = self.held != self.layout_held;
        if !nav && !capacity_changed && !lift_changed && (radius - self.radius).abs() < 3.0 {
            return;
        }
        if lift_changed {
            self.lift_graphic = self
                .held
                .filter(|&i| i < scan.nodes.len())
                .map(|id| (id, self.capture_graphic(scan, id)));
        }
        let adjusted = self.dragged_bytes(scan, focus);
        let original = scan.nodes[focus].bytes;
        let measured = adjusted.get(&focus).copied().unwrap_or(original);
        let (capacity, free) = volume.unwrap_or((measured, 0));
        // Holding a petal previews reclaiming its bytes. On a volume it grows
        // the free gap; inside a folder the remaining children fill the circle.
        let denominator = volume
            .map_or(measured, |(total, _)| total.max(original))
            .max(1);
        let per_byte = f64::from(TAU) / denominator as f64;
        let extent = per_byte * measured as f64;
        let unmapped = capacity.saturating_sub(free).saturating_sub(original);
        let end_angle = f64::from(TAU) - per_byte * unmapped as f64;
        let mut layout = Layout {
            scan,
            adjusted,
            root_bytes: measured.max(1) as f64,
            extent,
            start_angle: (end_angle - extent).max(0.0),
            radius,
            entries: Vec::new(),
        };
        layout.children(focus, 0.0, 0);
        if unmapped > 0 {
            layout.entries.push((
                Petal::Unmapped,
                Sector {
                    start: end_angle as f32,
                    end: TAU,
                    inner: HUB,
                    outer: BANDS[1],
                    color: UNMAPPED,
                },
            ));
        }
        // Very shallow trees still get an inspectable map on a large window.
        let last = layout
            .entries
            .iter()
            .map(|(_, p)| p.outer)
            .fold(HUB, f32::max);
        let expansion = ((0.74 - HUB) / (last - HUB).max(0.01)).max(1.0);
        for (_, s) in &mut layout.entries {
            s.inner = HUB + (s.inner - HUB) * expansion;
            s.outer = HUB + (s.outer - HUB) * expansion;
        }
        self.swatches.clear();
        self.swatches
            .insert(Swatch::Entry(focus), petal_color(0.5, 0));
        for &(petal, sector) in &layout.entries {
            let key = match petal {
                Petal::Unmapped => continue,
                Petal::Entry(id) => Swatch::Entry(id),
                Petal::Smaller { parent, .. } => Swatch::SmallerWithin(parent),
            };
            self.swatches.insert(key, sector.color);
        }
        let prev_t = self.progress(now);
        let mut old: HashMap<_, _> = self
            .motions
            .iter()
            .filter(|m| {
                (lift_changed && !nav)
                    || m.to_alpha > 0.0
                    || lerp(m.from_alpha, m.to_alpha, prev_t) > 0.001
            })
            .map(|m| {
                (
                    m.petal,
                    (
                        m.from.tween(m.to, prev_t),
                        lerp(m.from_alpha, m.to_alpha, prev_t),
                    ),
                )
            })
            .collect();
        let inward = old.get(&Petal::Entry(focus)).map(|v| v.0);
        let outward = self.focus.and_then(|previous| {
            layout
                .entries
                .iter()
                .find(|(k, _)| *k == Petal::Entry(previous))
                .map(|(_, s)| *s)
        });
        self.motions.clear();
        for (petal, to) in layout.entries {
            let (from, alpha) = old.remove(&petal).unwrap_or_else(|| {
                let start = if let Some(arc) = inward {
                    to.into_arc(arc)
                } else if self.focus.is_none() {
                    to.folded()
                } else {
                    Sector {
                        inner: to.outer,
                        ..to
                    }
                };
                (start, 0.0)
            });
            self.motions.push(Motion {
                petal,
                from,
                to,
                from_alpha: alpha,
                to_alpha: 1.0,
            });
        }
        if nav || lift_changed {
            for (petal, (from, alpha)) in old {
                let to = if let Some(arc) = outward {
                    from.into_arc(arc)
                } else if lift_changed && !nav {
                    let seam = (from.start + from.end) * 0.5;
                    Sector {
                        start: seam,
                        end: seam,
                        ..from
                    }
                } else {
                    Sector {
                        inner: HUB,
                        outer: HUB,
                        ..from
                    }
                };
                self.motions.push(Motion {
                    petal,
                    from,
                    to,
                    from_alpha: alpha,
                    to_alpha: 0.0,
                });
            }
        }
        if nav {
            self.small = None;
            self.hover_state = None;
            self.previous_hover = None;
            self.press = None;
        }
        self.volume = volume;
        self.layout_held = self.held;
        let label = volume.map_or(measured, |(total, _)| total);
        self.displayed_bytes = (
            if lift_changed {
                self.label_bytes(prev_t)
            } else {
                label
            },
            label,
        );
        self.duration = if lift_changed && !nav { 0.29 } else { 0.43 };
        self.started = if nav || capacity_changed || lift_changed {
            now
        } else {
            now - 1.0
        };
        self.focus = Some(focus);
        self.radius = radius;
    }

    fn progress(&self, now: f64) -> f32 {
        let t = ((now - self.started) / self.duration.max(0.001)).clamp(0.0, 1.0) as f32;
        t * t * (3.0 - 2.0 * t)
    }

    fn label_bytes(&self, t: f32) -> u64 {
        let (from, to) = self.displayed_bytes;
        (from as f64 + (to as f64 - from as f64) * f64::from(t)).round() as u64
    }

    fn petal_at(&self, p: Vec2, t: f32) -> Option<Petal> {
        let angle = p.angle().rem_euclid(TAU);
        let distance = p.length();
        self.motions.iter().find_map(|m| {
            let s = m.from.tween(m.to, t);
            (m.to_alpha > 0.0
                && angle >= s.start
                && angle < s.end
                && distance >= s.inner
                && distance < s.outer)
                .then_some(m.petal)
        })
    }

    fn drag_source(
        &self,
        p: Vec2,
        t: f32,
        disk: Option<(u64, u64)>,
        measured: u64,
    ) -> Option<ChartEvent> {
        if p.length() < HUB {
            return Some(ChartEvent::BlockedDrag(
                "The center represents the current folder.",
            ));
        }
        match self.petal_at(p, t) {
            Some(Petal::Entry(id)) => Some(ChartEvent::Drag(id)),
            Some(Petal::Smaller { .. }) => Some(ChartEvent::BlockedDrag(
                "Open smaller items to choose a file or folder.",
            )),
            Some(Petal::Unmapped) => Some(ChartEvent::BlockedDrag(
                "Unmapped space has no file to collect.",
            )),
            None => {
                let (total, free) = disk?;
                let gap = TAU
                    * (free.min(total.saturating_sub(measured)) as f64 / total.max(1) as f64)
                        as f32;
                (p.length() <= 1.0 && p.angle().rem_euclid(TAU) < gap)
                    .then_some(ChartEvent::BlockedDrag("Free space is already available."))
            }
        }
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        scan: &Scan,
        focus: usize,
        external_highlight: Option<usize>,
        volume: Option<(u64, u64)>,
        size: Vec2,
    ) -> ChartOutput {
        let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
        let center = rect.center();
        let radius = (size.x.min(size.y) * 0.475).max(1.0);
        let now = ui.input(|i| i.time);
        let volume = if focus == self.view_root {
            volume
        } else {
            None
        };
        self.update_layout(scan, focus, volume, radius, now);
        let t = self.progress(now);
        if t < 1.0 {
            ui.ctx().request_repaint();
        }
        let pointer = response.hover_pos().map(|pos| (pos - center) / radius);
        let inside_hub = pointer.is_some_and(|p| p.length() < HUB);
        let hit = pointer.and_then(|p| self.petal_at(p, t));
        let hovered = match hit {
            Some(Petal::Entry(id)) => Some(id),
            Some(Petal::Smaller { parent, .. }) if parent != focus => Some(parent),
            _ => None,
        };
        let mut output = ChartOutput {
            hovered,
            event: None,
        };
        if matches!(hit, Some(Petal::Entry(_) | Petal::Smaller { .. }))
            || (inside_hub && focus != self.view_root)
        {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let highlight = hit.or_else(|| external_highlight.map(Petal::Entry));
        if self.hover_state != highlight {
            self.previous_hover = self.hover_state;
            self.hover_state = highlight;
            self.hover_changed = now;
        }
        let mix = (((now - self.hover_changed) / 0.13) as f32).clamp(0.0, 1.0);
        if mix < 1.0 {
            ui.ctx().request_repaint();
        }
        let mut mesh = Mesh::default();
        for m in &self.motions {
            let alpha = lerp(m.from_alpha, m.to_alpha, t);
            if alpha < 0.004 {
                continue;
            }
            let s = m.from.tween(m.to, t);
            let glow = if Some(m.petal) == self.hover_state {
                mix
            } else if Some(m.petal) == self.previous_hover {
                1.0 - mix
            } else {
                0.0
            };
            let color = tint(s.color, Color32::WHITE, glow * 0.44).gamma_multiply(alpha);
            draw_sector(&mut mesh, s, color, center, radius);
        }
        let painter = ui.painter_at(rect);
        painter.add(mesh);
        let hub_radius = HUB * radius;
        let ring_color = if inside_hub && focus != self.view_root {
            Color32::from_gray(173)
        } else {
            Color32::from_rgb(84, 96, 118)
        };
        painter.circle_stroke(center, hub_radius - 0.5, Stroke::new(0.8_f32, ring_color));
        let label = format_size(self.label_bytes(t));
        let (number, unit) = label.rsplit_once(' ').unwrap_or((&label, ""));
        let font_size = (hub_radius * 0.46).clamp(9.0, 27.0);
        let fg = Color32::from_rgb(239, 243, 252);
        painter.text(
            center - Vec2::new(0.0, font_size * 0.28),
            Align2::CENTER_CENTER,
            number,
            FontId::proportional(font_size),
            fg,
        );
        painter.text(
            center + Vec2::new(0.0, font_size * 0.63),
            Align2::CENTER_CENTER,
            unit,
            FontId::proportional(font_size * 0.62),
            Color32::from_rgb(190, 201, 219),
        );

        if response.is_pointer_button_down_on()
            && ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Primary))
        {
            self.press = ui.input(|i| i.pointer.press_origin()).and_then(|pos| {
                self.drag_source((pos - center) / radius, t, volume, scan.nodes[focus].bytes)
            });
        }
        if response.drag_started_by(egui::PointerButton::Primary) {
            output.event = self.press.take();
        } else if response.clicked() {
            let collect = ui.input(|i| i.modifiers.shift || i.modifiers.command);
            match hit {
                Some(Petal::Entry(id)) => {
                    output.event = Some(if collect {
                        ChartEvent::Collect(id)
                    } else {
                        ChartEvent::Open(id)
                    })
                }
                Some(Petal::Smaller { parent, start }) => self.small = Some((parent, start)),
                None if inside_hub && focus != self.view_root => {
                    output.event = Some(ChartEvent::Back)
                }
                _ => {}
            }
        }
        if !response.dragged() && (hit.is_some() || (inside_hub && focus != self.view_root)) {
            response.on_hover_ui_at_pointer(|ui| match hit {
                Some(Petal::Unmapped) => {
                    let (total, free) = volume.unwrap_or_default();
                    let bytes = total
                        .saturating_sub(free)
                        .saturating_sub(scan.nodes[focus].bytes);
                    ui.label("Unmapped space");
                    ui.weak(format_size(bytes));
                }
                Some(Petal::Entry(id)) => {
                    ui.label(scan.nodes[id].name.to_string_lossy());
                    ui.weak(format_size(scan.nodes[id].bytes));
                }
                Some(Petal::Smaller { parent, start }) => {
                    ui.label(format!(
                        "{} smaller items",
                        scan.nodes[parent].children.len() - start
                    ));
                }
                None if inside_hub && focus != self.view_root => {
                    ui.label("Go to parent");
                }
                _ => {}
            });
        }
        if let Some((parent, start)) = self.small {
            let mut open = true;
            let children = &scan.nodes[parent].children[start..];
            egui::Window::new("Smaller items")
                .open(&mut open)
                .collapsible(false)
                .default_width(350.0)
                .show(ui.ctx(), |ui| {
                    egui::ScrollArea::vertical().max_height(320.0).show_rows(
                        ui,
                        23.0,
                        children.len(),
                        |ui, rows| {
                            for &id in &children[rows] {
                                ui.horizontal(|ui| {
                                    let item = ui.selectable_label(
                                        false,
                                        scan.nodes[id].name.to_string_lossy(),
                                    );
                                    if item.clicked() {
                                        output.event = Some(ChartEvent::Open(id));
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.weak(format_size(scan.nodes[id].bytes));
                                        },
                                    );
                                });
                            }
                        },
                    );
                });
            if !open || output.event.is_some() {
                self.small = None;
            }
        }
        output
    }
}

// Four concentric vertices per angle provide a subpixel feather at both circular
// edges. A single mesh keeps densely fragmented disks inexpensive to render.
fn draw_sector(mesh: &mut Mesh, s: Sector, color: Color32, center: Pos2, radius: f32) {
    // Neighboring petals share a fine hairline, including the straight cuts
    // between siblings. Feather both directions for fractional display scales.
    let outer = s.outer * radius - 0.78;
    let inner = s.inner * radius + 0.78;
    if outer <= inner {
        return;
    }
    let mid = ((outer + inner) * 0.5).max(1.0);
    let gap = 0.14 / mid;
    let start = s.start + gap;
    let end = s.end - gap;
    if end <= start {
        return;
    }
    let cap = (0.78 / mid).min((end - start) * 0.45);
    let steps = ((end - start) * (outer * 0.85).sqrt()).ceil().max(1.0) as u32;
    let first = mesh.vertices.len() as u32;
    for i in 0..=steps + 2 {
        let (angle, fill) = if i == 0 {
            (start, Color32::TRANSPARENT)
        } else if i == steps + 2 {
            (end, Color32::TRANSPARENT)
        } else {
            (
                lerp(start + cap, end - cap, (i - 1) as f32 / steps as f32),
                color,
            )
        };
        let ray = Vec2::angled(angle);
        mesh.colored_vertex(center + ray * (inner - 0.7), Color32::TRANSPARENT);
        mesh.colored_vertex(center + ray * inner, fill);
        mesh.colored_vertex(center + ray * outer, fill);
        mesh.colored_vertex(center + ray * (outer + 0.7), Color32::TRANSPARENT);
        if i == 0 {
            continue;
        }
        let base = first + (i - 1) * 4;
        for j in 0..3 {
            mesh.add_triangle(base + j, base + 1 + j, base + 4 + j);
            mesh.add_triangle(base + 1 + j, base + 5 + j, base + 4 + j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Node;
    use egui::{
        CentralPanel, Context, Event, Frame, Modifiers, PointerButton, RawInput, Rect, pos2, vec2,
    };

    fn fixture() -> Scan {
        let node = |name: &str, parent, bytes, children: Vec<usize>| Node {
            name: name.into(),
            bytes,
            is_dir: !children.is_empty(),
            identity: (0, 0, 0, 0),
            incomplete: false,
            parent,
            children,
        };
        Scan {
            root: "/volume".into(),
            skipped: 0,
            nodes: vec![
                node("volume", None, 100, vec![1, 2]),
                node("album", Some(0), 60, vec![3]),
                node("archive", Some(0), 40, vec![]),
                node("song", Some(1), 60, vec![]),
            ],
        }
    }

    #[test]
    fn zoom_preserves_visible_child_geometry_and_free_space() {
        let scan = fixture();
        let mut chart = Chart::default();
        chart.update_layout(&scan, 0, None, 180., 0.);
        chart.update_layout(&scan, 0, Some((200, 80)), 180., 0.5);
        let previous = chart
            .motions
            .iter()
            .find(|m| m.petal == Petal::Entry(3))
            .unwrap()
            .to;
        assert!((previous.start - TAU * 0.4).abs() < 1e-5);
        let unmapped = chart
            .motions
            .iter()
            .find(|m| m.petal == Petal::Unmapped)
            .unwrap();
        assert!((unmapped.to.end - unmapped.to.start - TAU * 0.1).abs() < 1e-5);
        chart.update_layout(&scan, 1, None, 180., 1.);
        let child = chart
            .motions
            .iter()
            .find(|m| m.petal == Petal::Entry(3))
            .unwrap();
        assert_eq!(child.from.start, previous.start);
        assert_eq!(child.from.inner, previous.inner);
        assert_eq!(child.to.start, 0.);
        assert_eq!(child.to.end, TAU);
        assert!(child.to.inner < child.from.inner);
        let halfway = chart.progress(1.215);
        assert!((halfway - 0.5).abs() < 1e-5);
        assert_eq!(chart.progress(2.), 1.);
    }

    fn motion(chart: &Chart, key: Petal) -> Motion {
        *chart.motions.iter().find(|m| m.petal == key).unwrap()
    }

    fn near(a: f32, b: f32) {
        assert!((a - b).abs() < 0.00001, "{a} != {b}");
    }

    #[test]
    fn lifting_changes_ancestor_sizes_and_restores_from_the_current_frame() {
        let mut scan = fixture();
        let mut extra = scan.nodes[3].clone();
        extra.name = "second track".into();
        extra.bytes = 24;
        scan.nodes[3].bytes = 36;
        scan.nodes.push(extra);
        scan.nodes[1].children.push(4);
        let mut chart = Chart::default();
        chart.update_layout(&scan, 0, None, 200., 0.);
        let before = motion(&chart, Petal::Entry(1)).to;
        chart.set_dragged(Some(3));
        chart.update_layout(&scan, 0, None, 200., 1.);
        let changed = motion(&chart, Petal::Entry(1));
        near(changed.from.start, before.start);
        near(changed.to.end - changed.to.start, TAU * 24. / 64.);
        near(changed.to.start, TAU * 40. / 64.);
        let fading = motion(&chart, Petal::Entry(3));
        assert_eq!(fading.to_alpha, 0.);
        assert_eq!(chart.label_bytes(1.), 64);

        let fraction = chart.progress(1.11);
        assert!(fraction > 0. && fraction < 1.);
        let midway = fading.from.tween(fading.to, fraction);
        chart.set_dragged(None);
        chart.update_layout(&scan, 0, None, 200., 1.11);
        let restoring = motion(&chart, Petal::Entry(3));
        near(restoring.from.start, midway.start);
        near(restoring.from.end, midway.end);
        near(restoring.from_alpha, 1. - fraction);
        near(motion(&chart, Petal::Entry(1)).to.start, before.start);
        near(motion(&chart, Petal::Entry(1)).to.end, before.end);
        assert_eq!(chart.label_bytes(1.), 100);
        assert_eq!(scan.nodes[0].bytes, 100);
    }

    #[test]
    fn whole_disk_drag_increases_free_gap_and_lifts_matching_branch_colors() {
        let scan = fixture();
        let mut chart = Chart::default();
        let volume = Some((200, 80));
        chart.update_layout(&scan, 0, volume, 200., 0.);
        let color = motion(&chart, Petal::Entry(1)).to.color;
        chart.set_dragged(Some(1));
        chart.update_layout(&scan, 0, volume, 200., 1.);
        let remaining = motion(&chart, Petal::Entry(2)).to;
        near(remaining.start, TAU * 140. / 200.);
        near(remaining.end - remaining.start, TAU * 40. / 200.);
        let unmapped = motion(&chart, Petal::Unmapped).to;
        near(unmapped.end - unmapped.start, TAU * 20. / 200.);
        let (id, cluster) = chart.lift_graphic.as_ref().unwrap();
        assert_eq!(*id, 1);
        assert_eq!(chart.branch_color(&scan, 1), color);
        assert_eq!(cluster.len(), 2);
        assert!(cluster.iter().any(|s| s.color == color));
        assert!(cluster.iter().any(|s| s.color == FILE));
        assert!(
            cluster
                .iter()
                .all(|s| s.inner >= 0.17 && s.outer <= 1.001 && s.inner < s.outer)
        );
        assert_eq!(motion(&chart, Petal::Entry(3)).to_alpha, 0.);
        chart.set_dragged(None);
        chart.update_layout(&scan, 0, volume, 200., 2.);
        near(
            motion(&chart, Petal::Entry(1)).from.start,
            motion(&chart, Petal::Entry(1)).from.end,
        );
        near(motion(&chart, Petal::Entry(1)).to.start, TAU * 0.4);
        assert_eq!(chart.label_bytes(1.), 200);
    }

    #[test]
    fn free_hidden_center_and_aggregate_report_virtual_sources() {
        let mut scan = fixture();
        let mut tiny = scan.nodes[2].clone();
        tiny.bytes = 1;
        scan.nodes.push(tiny);
        scan.nodes[0].children.push(4);
        scan.nodes[0].bytes += 1;
        let disk = Some((200, 80));
        let mut chart = Chart::default();
        chart.update_layout(&scan, 0, disk, 100., 0.);
        let hit = |p| chart.drag_source(p, 1., disk, 101);
        assert!(matches!(hit(Vec2::ZERO), Some(ChartEvent::BlockedDrag(_))));
        assert!(matches!(
            hit(Vec2::angled(0.4) * 0.4),
            Some(ChartEvent::BlockedDrag("Free space is already available."))
        ));
        let at = |s: Sector| Vec2::angled((s.start + s.end) * 0.5) * ((s.outer + s.inner) * 0.5);
        assert!(matches!(
            hit(at(motion(&chart, Petal::Unmapped).to)),
            Some(ChartEvent::BlockedDrag(
                "Unmapped space has no file to collect."
            ))
        ));
        assert!(matches!(
            hit(at(motion(
                &chart,
                Petal::Smaller {
                    parent: 0,
                    start: 2
                }
            )
            .to)),
            Some(ChartEvent::BlockedDrag(_))
        ));
        assert!(hit(Vec2::new(1.5, 0.)).is_none());
    }

    #[test]
    fn map_hover_drill_back_and_drag_start() {
        let ctx = Context::default();
        let scan = fixture();
        let mut chart = Chart::default();
        let mut frame = |focus, time, events| {
            let mut result = ChartOutput::default();
            let raw = RawInput {
                time: Some(time),
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(300., 240.))),
                events,
                ..RawInput::default()
            };
            let _ = ctx.run(raw, |ctx| {
                CentralPanel::default().frame(Frame::NONE).show(ctx, |ui| {
                    result = chart.show(ui, &scan, focus, None, None, vec2(200., 200.));
                });
            });
            result
        };
        let button = |pos, pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::default(),
        };
        frame(0, 0., vec![]);
        let petal = pos2(100., 130.);
        assert_eq!(
            frame(0, 1., vec![Event::PointerMoved(petal)]).hovered,
            Some(1)
        );
        frame(0, 1.01, vec![button(petal, true)]);
        assert!(matches!(
            frame(0, 1.02, vec![button(petal, false)]).event,
            Some(ChartEvent::Open(1))
        ));
        frame(1, 1.03, vec![]);
        let hub = pos2(100., 100.);
        frame(1, 1.5, vec![Event::PointerMoved(hub), button(hub, true)]);
        assert!(matches!(
            frame(1, 1.51, vec![button(hub, false)]).event,
            Some(ChartEvent::Back)
        ));
        frame(0, 1.52, vec![]);
        frame(0, 2., vec![Event::PointerMoved(petal), button(petal, true)]);
        assert!(matches!(
            frame(0, 2.1, vec![Event::PointerMoved(pos2(240., 220.))]).event,
            Some(ChartEvent::Drag(1))
        ));
        frame(0, 2.2, vec![button(pos2(240., 220.), false)]);
        frame(0, 2.3, vec![Event::PointerMoved(hub), button(hub, true)]);
        assert!(matches!(
            frame(0, 2.4, vec![Event::PointerMoved(pos2(20., 20.))]).event,
            Some(ChartEvent::BlockedDrag(_))
        ));
    }
}
