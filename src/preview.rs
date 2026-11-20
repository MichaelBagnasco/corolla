//! Load a small Quick Look preview outside the paint loop.
use std::{
    fs::OpenOptions,
    io::{BufReader, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
};

enum Content {
    Image(egui::ColorImage),
    Text(String),
}

fn read_preview(path: &Path) -> Option<Content> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut reader = image::ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .ok()?;
    if matches!(
        reader.format(),
        Some(image::ImageFormat::Jpeg | image::ImageFormat::Png)
    ) {
        if meta.len() > 48 * 1024 * 1024 {
            return None;
        }
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(96 * 1024 * 1024);
        limits.max_image_width = Some(12000);
        limits.max_image_height = Some(12000);
        reader.limits(limits);
        let thumbnail = reader.decode().ok()?.thumbnail(768, 480).to_rgba8();
        let size = [thumbnail.width() as usize, thumbnail.height() as usize];
        return Some(Content::Image(egui::ColorImage::from_rgba_unmultiplied(
            size,
            thumbnail.as_raw(),
        )));
    }
    let mut bytes = Vec::new();
    reader
        .into_inner()
        .take(4096)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    if text.is_empty()
        || text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        return None;
    }
    Some(Content::Text(
        text.lines().take(16).collect::<Vec<_>>().join("\n"),
    ))
}

#[derive(Default)]
pub struct Preview {
    path: PathBuf,
    loading: Option<Receiver<Option<Content>>>,
    image: Option<egui::TextureHandle>,
    text: Option<String>,
}

impl Preview {
    pub fn show(&mut self, ui: &mut egui::Ui, path: &Path) {
        if self.path != path {
            let (tx, rx) = mpsc::channel();
            *self = Self {
                path: path.to_owned(),
                loading: Some(rx),
                ..Default::default()
            };
            let path = self.path.clone();
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
                let _ = tx.send(read_preview(&path));
                ctx.request_repaint();
            });
        }
        if let Some(result) = self.loading.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.loading = None;
            match result {
                Some(Content::Image(pixels)) => {
                    self.image = Some(ui.ctx().load_texture(
                        "file-preview",
                        pixels,
                        egui::TextureOptions::LINEAR,
                    ))
                }
                Some(Content::Text(text)) => self.text = Some(text),
                None => {}
            }
        }
        if let Some(texture) = &self.image {
            let max = egui::vec2(ui.available_width().clamp(180.0, 420.0), 210.0);
            ui.add(
                egui::Image::new(texture)
                    .max_size(max)
                    .maintain_aspect_ratio(true),
            );
        } else if let Some(text) = &self.text {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(text).monospace()).wrap());
                    });
            });
        }
    }
}
