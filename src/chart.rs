use crate::{app::size as format_size, scan::Scan};
use egui::{Align2, Color32, CursorIcon, FontId, Mesh, Pos2, Sense, Stroke, Ui, Vec2};
use std::{collections::HashMap, f32::consts::TAU};

const HUB: f32 = 0.17;
const BANDS: [f32; 9] = [HUB, 0.30, 0.43, 0.56, 0.69, 0.82, 0.915, 0.967, 1.0];
pub const UNMAPPED: Color32 = Color32::from_rgb(149, 94, 182);
const FILE: Color32 = Color32::from_rgb(124, 134, 152);

pub enum ChartEvent {
    Open(usize),
    Back,
    Drag(usize),
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
        let children = &self.scan.nodes[parent].children;
        for (index, &id) in children.iter().enumerate() {
            let node = &self.scan.nodes[id];
            if node.bytes == 0 {
                break;
            }
            let span = node.bytes as f64 / self.root_bytes;
            let inner = BANDS[depth];
            let outer = BANDS[depth + 1];
            let tiny = span * self.extent * f64::from(outer * self.radius) < 2.3;
            let (petal, span, color) = if tiny {
                let bytes = children[index..]
                    .iter()
                    .map(|&i| self.scan.nodes[i].bytes as f64)
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
    focus: Option<usize>,
    radius: f32,
    motions: Vec<Motion>,
    volume: Option<(u64, u64)>,
    swatches: HashMap<Swatch, Color32>,
    started: f64,
    press: Option<usize>,
    small: Option<(usize, usize)>,
    hover_state: Option<Petal>,
    previous_hover: Option<Petal>,
    hover_changed: f64,
}

impl Chart {
    pub fn branch_color(&self, scan: &Scan, id: usize) -> Color32 {
        if !scan.nodes[id].is_dir {
            return FILE;
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
        if !nav && !capacity_changed && (radius - self.radius).abs() < 3.0 {
            return;
        }
        let bytes = scan.nodes[focus].bytes.max(1) as f64;
        let measured = scan.nodes[focus].bytes;
        let (capacity, free) = volume.unwrap_or((measured, 0));
        let per_byte = f64::from(TAU) / (capacity as f64).max(bytes);
        let extent = per_byte * measured as f64;
        let unmapped = capacity.saturating_sub(free).saturating_sub(measured);
        let end_angle = f64::from(TAU) - per_byte * unmapped as f64;
        let mut layout = Layout {
            scan,
            root_bytes: bytes,
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
            .filter(|m| m.to_alpha > 0.0)
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
        if nav {
            for (petal, (from, alpha)) in old {
                let to = if let Some(arc) = outward {
                    from.into_arc(arc)
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
            self.small = None;
            self.hover_state = None;
            self.previous_hover = None;
            self.press = None;
        }
        self.volume = volume;
        self.started = if nav || capacity_changed {
            now
        } else {
            now - 1.0
        };
        self.focus = Some(focus);
        self.radius = radius;
    }

    fn progress(&self, now: f64) -> f32 {
        let t = ((now - self.started) as f32 / 0.43).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
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
        let volume = if focus == 0 { volume } else { None };
        self.update_layout(scan, focus, volume, radius, now);
        let t = self.progress(now);
        if t < 1.0 {
            ui.ctx().request_repaint();
        }
        let pointer = response.hover_pos().map(|pos| (pos - center) / radius);
        let inside_hub = pointer.is_some_and(|p| p.length() < HUB);
        let hit = pointer.and_then(|pointer| {
            let angle = pointer.angle().rem_euclid(TAU);
            let distance = pointer.length();
            self.motions.iter().find_map(|m| {
                let s = m.from.tween(m.to, t);
                (m.to_alpha > 0.0
                    && angle >= s.start
                    && angle < s.end
                    && distance >= s.inner
                    && distance < s.outer)
                    .then_some(m.petal)
            })
        });
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
            || (inside_hub && focus != 0)
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
        let ring_color = if inside_hub && focus != 0 {
            Color32::from_gray(173)
        } else {
            Color32::from_rgb(84, 96, 118)
        };
        painter.circle_stroke(center, hub_radius - 0.5, Stroke::new(0.8_f32, ring_color));
        let label = format_size(volume.map_or(scan.nodes[focus].bytes, |(total, _)| total));
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

        if response.is_pointer_button_down_on() && ui.input(|i| i.pointer.any_pressed()) {
            self.press = match hit {
                Some(Petal::Entry(id)) => Some(id),
                _ => None,
            };
        }
        if response.drag_started() {
            output.event = self.press.take().map(ChartEvent::Drag);
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
                None if inside_hub && focus != 0 => output.event = Some(ChartEvent::Back),
                _ => {}
            }
        }
        if !response.dragged() && (hit.is_some() || (inside_hub && focus != 0)) {
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
                None if inside_hub && focus != 0 => {
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
    }
}
