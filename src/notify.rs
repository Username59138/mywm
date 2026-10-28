//! Messages on screen: config errors and warnings found by the core, and
//! whatever the config shows with `wm.notify`. They stack in the top right
//! corner of every output, above the scene; a click dismisses one.

use std::{
    process::Command,
    time::{Duration, Instant},
};

use fontdue::{Font, FontSettings};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::element::{
            Kind,
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        },
        renderer::gles::GlesRenderer,
    },
    utils::{Logical, Point, Rectangle, Scale, Transform},
};
use tracing::warn;

use crate::render::SceneElement;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "info" => Some(Level::Info),
            "warn" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }

    pub fn default_timeout(self) -> Option<Duration> {
        match self {
            Level::Info => Some(Duration::from_secs(5)),
            Level::Warn => Some(Duration::from_secs(10)),
            // an error stays until it is clicked or the config is reloaded
            Level::Error => None,
        }
    }

    /// The stripe on the left, premultiplied RGBA.
    fn accent(self) -> [u8; 4] {
        match self {
            Level::Info => [0x89, 0xb4, 0xfa, 0xff],
            Level::Warn => [0xf9, 0xe2, 0xaf, 0xff],
            Level::Error => [0xf3, 0x8b, 0xa8, 0xff],
        }
    }

    fn label(self) -> &'static str {
        match self {
            Level::Info => "",
            Level::Warn => "warning: ",
            Level::Error => "error: ",
        }
    }
}

const FONT_SIZE: f32 = 15.0;
const MAX_WIDTH: usize = 620;
const MAX_LINES: usize = 12;
const PAD: usize = 10;
const STRIPE: usize = 4;
const MARGIN: i32 = 12;
const SPACING: i32 = 8;
const MAX_NOTES: usize = 6;
const BACKGROUND: [u8; 4] = [0x1e, 0x1e, 0x2e, 0xf0];
const TEXT: [u8; 3] = [0xcd, 0xd6, 0xf4];

struct Note {
    level: Level,
    text: String,
    /// the same message again and again (a handler that fails every frame)
    count: u32,
    expires: Option<Instant>,
    /// the config generation that showed it; None for the core's own
    owner: Option<u32>,
    image: Option<(MemoryRenderBuffer, (i32, i32))>,
    /// the count and time the image shows
    drawn: (u32, Instant),
}

#[derive(Default)]
pub struct Notes {
    list: Vec<Note>,
    font: Option<Option<Font>>,
    /// where each note was drawn last, relative to its output's top right
    /// corner: for clicks
    rects: Vec<Rectangle<i32, Logical>>,
}

impl Notes {
    pub fn push(&mut self, level: Level, text: &str, timeout: Option<Duration>, owner: Option<u32>) {
        let text = text.trim_end().to_string();
        let expires = timeout.map(|t| Instant::now() + t);
        if let Some(n) = self.list.iter_mut().find(|n| n.level == level && n.text == text) {
            n.count += 1;
            n.expires = expires;
            return;
        }
        let drawn = (0, Instant::now());
        self.list.push(Note { level, text, count: 1, expires, owner, image: None, drawn });
        if self.list.len() > MAX_NOTES {
            self.list.remove(0);
        }
    }

    /// Drop the notes that timed out.
    pub fn expire(&mut self, now: Instant) {
        self.list.retain(|n| n.expires.is_none_or(|t| t > now));
    }

    /// Config `generation` is now running: what older configs showed and
    /// the core's errors about them are no longer current.
    pub fn clear_config(&mut self, generation: u32) {
        self.list.retain(|n| match n.owner {
            Some(g) => g == generation,
            None => n.level != Level::Error,
        });
    }

    /// What a config that failed to load showed.
    pub fn drop_owner(&mut self, generation: u32) {
        self.list.retain(|n| n.owner != Some(generation));
    }

    pub fn clear(&mut self) {
        self.list.clear();
    }

    /// A click at `pos` relative to an output's top right corner. Returns
    /// true if it hit (and dismissed) a note.
    pub fn click(&mut self, pos: Point<f64, Logical>) -> bool {
        let pos = pos.to_i32_round();
        let Some(i) = self.rects.iter().position(|r| r.contains(pos)) else { return false };
        if i < self.list.len() {
            self.list.remove(i);
        }
        self.rects.clear();
        true
    }

    /// The notes for one output, front-to-back; `size` is its logical size.
    pub fn elements(&mut self, renderer: &mut GlesRenderer, origin: Point<f64, Logical>, width: i32, scale: Scale<f64>) -> Vec<SceneElement> {
        if self.list.is_empty() {
            self.rects.clear();
            return Vec::new();
        }
        if self.font.is_none() {
            self.font = Some(load_font());
        }
        let Some(Some(font)) = &self.font else { return Vec::new() };

        let mut elements = Vec::new();
        let mut rects = Vec::new();
        let mut y = MARGIN;
        // newest on top
        for note in self.list.iter_mut().rev() {
            // a count that runs up every frame is redrawn a few times a second
            let stale = note.drawn.0 != note.count && note.drawn.1.elapsed() > Duration::from_millis(250);
            if note.image.is_none() || stale {
                note.image = Some(draw_note(font, note));
                note.drawn = (note.count, Instant::now());
            }
            let (buffer, (w, h)) = note.image.as_ref().unwrap();
            let x = width - MARGIN - w;
            rects.push(Rectangle::new((x - width, y).into(), (*w, *h).into()));
            let loc = (origin + Point::from((x as f64, y as f64))).to_physical_precise_round(scale);
            match MemoryRenderBufferRenderElement::from_buffer(renderer, loc, buffer, None, None, None, Kind::Unspecified) {
                Ok(e) => elements.push(e.into()),
                Err(e) => warn!("cannot draw a notification: {e}"),
            }
            y += h + SPACING;
        }
        // rects are relative to the top right corner, in list order
        rects.reverse();
        self.rects = rects;
        elements
    }
}

/// A monospace font from fontconfig, else a few usual paths.
fn load_font() -> Option<Font> {
    let mut paths = Vec::new();
    if let Ok(out) = Command::new("fc-match").args(["-f", "%{file}", "monospace"]).output()
        && out.status.success()
    {
        paths.push(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    paths.extend(
        [
            "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
        ]
        .map(String::from),
    );
    for path in paths {
        let Ok(data) = std::fs::read(&path) else { continue };
        match Font::from_bytes(data, FontSettings::default()) {
            Ok(font) => return Some(font),
            Err(e) => warn!("cannot use the font {path}: {e}"),
        }
    }
    warn!("no font found for notifications; they go to the log only");
    None
}

/// Break text into lines that fit `max` pixels.
fn wrap(font: &Font, text: &str, max: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.replace('\t', "    ");
        let mut line = String::new();
        let mut width = 0.0;
        for ch in raw.chars() {
            let adv = font.metrics(ch, FONT_SIZE).advance_width;
            if width + adv > max && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                width = 0.0;
            }
            line.push(ch);
            width += adv;
        }
        lines.push(line);
    }
    lines
}

fn draw_note(font: &Font, note: &Note) -> (MemoryRenderBuffer, (i32, i32)) {
    let mut text = format!("{}{}", note.level.label(), note.text);
    if note.count > 1 {
        text.push_str(&format!("  (x{})", note.count));
    }
    let inner = (MAX_WIDTH - 2 * PAD - STRIPE) as f32;
    let mut lines = wrap(font, &text, inner);
    if lines.len() > MAX_LINES {
        lines.truncate(MAX_LINES);
        lines.push("...".into());
    }

    let metrics = font.horizontal_line_metrics(FONT_SIZE);
    let (ascent, line_h) = metrics.map_or((FONT_SIZE, FONT_SIZE * 1.3), |m| (m.ascent, m.new_line_size));
    let text_w = lines
        .iter()
        .map(|l| l.chars().map(|c| font.metrics(c, FONT_SIZE).advance_width).sum::<f32>())
        .fold(0.0, f32::max);
    let w = (STRIPE + 2 * PAD + text_w.ceil() as usize).min(MAX_WIDTH);
    let h = 2 * PAD + (line_h * lines.len() as f32).ceil() as usize;

    // RGBA bytes, premultiplied (Abgr8888 in DRM's little-endian naming)
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let c = if x < STRIPE { note.level.accent() } else { BACKGROUND };
            let a = c[3] as u32;
            let i = (y * w + x) * 4;
            px[i] = (c[0] as u32 * a / 255) as u8;
            px[i + 1] = (c[1] as u32 * a / 255) as u8;
            px[i + 2] = (c[2] as u32 * a / 255) as u8;
            px[i + 3] = c[3];
        }
    }
    for (row, line) in lines.iter().enumerate() {
        let baseline = PAD as f32 + ascent + row as f32 * line_h;
        let mut pen = (STRIPE + PAD) as f32;
        for ch in line.chars() {
            let (m, bitmap) = font.rasterize(ch, FONT_SIZE);
            let gx = (pen + m.xmin as f32).round() as i64;
            let gy = (baseline - m.height as f32 - m.ymin as f32).round() as i64;
            for by in 0..m.height {
                for bx in 0..m.width {
                    let cov = bitmap[by * m.width + bx] as u32;
                    let (x, y) = (gx + bx as i64, gy + by as i64);
                    if cov == 0 || x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
                        continue;
                    }
                    let i = (y as usize * w + x as usize) * 4;
                    for k in 0..3 {
                        let fg = TEXT[k] as u32 * cov / 255;
                        px[i + k] = (fg + px[i + k] as u32 * (255 - cov) / 255) as u8;
                    }
                    px[i + 3] = (cov + px[i + 3] as u32 * (255 - cov) / 255) as u8;
                }
            }
            pen += m.advance_width;
        }
    }
    let buffer = MemoryRenderBuffer::from_slice(&px, Fourcc::Abgr8888, (w as i32, h as i32), 1, Transform::Normal, None);
    (buffer, (w as i32, h as i32))
}
