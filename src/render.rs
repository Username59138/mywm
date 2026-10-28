//! Turning the scene into render elements; shared by both backends.

use std::{collections::HashSet, time::Duration};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                AsRenderElements, Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                render_elements,
                solid::SolidColorRenderElement,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
                texture::TextureRenderElement,
            },
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::{space::SpaceElement, utils::send_frames_surface_tree},
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    utils::{IsAlive, Logical, Point, Rectangle, Scale, Transform},
    wayland::compositor::with_states,
};
use tracing::warn;

use crate::{
    scene::{DrawItem, NodeKind},
    state::{Shared, Stage},
};

pub const CLEAR_COLOR: [f32; 4] = [0.11, 0.11, 0.12, 1.0];

render_elements! {
    pub SceneElement<=GlesRenderer>;
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
    Solid = SolidColorRenderElement,
    Texture = TextureRenderElement<GlesTexture>,
    Memory = MemoryRenderBufferRenderElement<GlesRenderer>,
}

/// The scene as seen by an output whose top left corner is `origin`, in
/// front-to-back order (what the renderers expect).
pub fn scene_elements(
    sh: &Shared,
    renderer: &mut GlesRenderer,
    origin: Point<f64, Logical>,
    scale: Scale<f64>,
) -> Vec<SceneElement> {
    let mut elements = Vec::new();
    // X11 menus and tooltips first: they are above the whole scene
    for w in sh.or_windows.iter().rev() {
        let Some(x) = w.x11_surface() else { continue };
        let loc = (x.geometry().loc.to_f64() - origin).to_physical_precise_round(scale);
        elements.extend(w.render_elements::<SceneElement>(renderer, loc, scale, 1.0));
    }
    for item in sh.scene.draw_list().iter().rev() {
        match *item {
            DrawItem::Rect { node, x, y, alpha } => {
                if let NodeKind::Rect { buffer, .. } = &sh.scene.nodes[node].kind {
                    let loc = Point::<f64, Logical>::from((x - origin.x, y - origin.y)).to_physical_precise_round(scale);
                    elements.push(SolidColorRenderElement::from_buffer(buffer, loc, scale, alpha, Kind::Unspecified).into());
                }
            }
            DrawItem::Layer { layer, x, y, alpha } => {
                let Some(l) = sh.layers.get(&layer).filter(|l| l.stage == Stage::Ready) else { continue };
                let loc = Point::<f64, Logical>::from((x - origin.x, y - origin.y)).to_physical_precise_round(scale);
                elements.extend(l.surface.render_elements::<SceneElement>(renderer, loc, scale, alpha));
            }
            DrawItem::Client { client, x, y, alpha } => {
                let Some(e) = sh.clients.get(&client) else { continue };
                if let Some(snap) = &e.snapshot {
                    let loc = Point::<f64, Logical>::from((
                        x - origin.x + snap.offset.x as f64,
                        y - origin.y + snap.offset.y as f64,
                    ))
                    .to_physical(scale);
                    elements.push(
                        TextureRenderElement::from_texture_buffer(
                            loc,
                            &snap.buffer,
                            Some(alpha),
                            None,
                            Some(snap.size),
                            Kind::Unspecified,
                        )
                        .into(),
                    );
                } else if e.stage == Stage::Ready {
                    let geo = e.window.geometry();
                    let loc = Point::<f64, Logical>::from((
                        x - origin.x - geo.loc.x as f64,
                        y - origin.y - geo.loc.y as f64,
                    ))
                    .to_physical_precise_round(scale);
                    elements.extend(e.window.render_elements::<SceneElement>(renderer, loc, scale, alpha));
                }
            }
        }
    }
    elements
}

/// Frame callbacks for the windows shown on this output, and enter/leave
/// events so clients know which output (and scale) they are on. Windows
/// that are not on screen get no frame callbacks, so they stop drawing
/// until they are shown.
pub fn send_frames(sh: &Shared, output: &Output, area: Rectangle<f64, Logical>, time: Duration) {
    let mut shown = HashSet::new();
    for item in sh.scene.draw_list() {
        if let DrawItem::Layer { layer, .. } = item {
            if let Some(l) = sh.layers.get(&layer).filter(|l| l.stage == Stage::Ready) {
                l.surface.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
            }
            continue;
        }
        let DrawItem::Client { client, x, y, .. } = item else { continue };
        let Some(e) = sh.clients.get(&client) else { continue };
        if e.stage != Stage::Ready || e.snapshot.is_some() {
            continue;
        }
        let rect = Rectangle::new((x, y).into(), e.window.geometry().size.to_f64());
        let Some(mut overlap) = rect.intersection(area) else { continue };
        overlap.loc -= rect.loc;
        e.window.output_enter(output, overlap.to_i32_round());
        e.window.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
        shown.insert(client);
    }
    for (id, e) in &sh.clients {
        if e.stage == Stage::Ready && !shown.contains(id) {
            e.window.output_leave(output);
        }
    }
    for w in &sh.or_windows {
        let Some(x) = w.x11_surface() else { continue };
        let geo = x.geometry().to_f64();
        if let Some(mut overlap) = geo.intersection(area) {
            overlap.loc -= geo.loc;
            w.output_enter(output, overlap.to_i32_round());
            w.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
        }
    }
}

/// The pointer image when the focused client does not set its own.
pub struct Cursor {
    buffer: MemoryRenderBuffer,
    hotspot: Point<i32, Logical>,
}

impl Cursor {
    /// The "default" cursor of a theme (XCURSOR_THEME, else "default") at a
    /// size (XCURSOR_SIZE, else 24), or a plain arrow when the theme cannot
    /// be read.
    pub fn load(theme: Option<String>, size: Option<String>) -> Cursor {
        let theme = theme.unwrap_or_else(|| "default".into());
        let size: u32 = size.and_then(|s| s.parse().ok()).unwrap_or(24);

        let image = xcursor::CursorTheme::load(&theme)
            .load_icon("default")
            .or_else(|| xcursor::CursorTheme::load(&theme).load_icon("left_ptr"))
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|data| xcursor::parser::parse_xcursor(&data))
            .and_then(|images| {
                images
                    .into_iter()
                    .min_by_key(|i| (i.size as i64 - size as i64).abs())
            });

        match image {
            Some(i) => Cursor {
                buffer: MemoryRenderBuffer::from_slice(
                    &i.pixels_rgba,
                    Fourcc::Abgr8888,
                    (i.width as i32, i.height as i32),
                    1,
                    Transform::Normal,
                    None,
                ),
                hotspot: (i.xhot as i32, i.yhot as i32).into(),
            },
            None => {
                warn!("cannot load the '{theme}' cursor theme, using a plain arrow");
                fallback_arrow()
            }
        }
    }
}

fn fallback_arrow() -> Cursor {
    let (w, h) = (12usize, 18usize);
    let mut pixels = vec![0u8; w * h * 4];
    let inside = |x: f32, y: f32| y < 16.0 && x <= y * 0.7;
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32, y as f32);
            if !inside(fx, fy) {
                continue;
            }
            let edge = !inside(fx + 1.0, fy) || !inside(fx, fy + 1.0) || x == 0;
            let v = if edge { 0 } else { 255 };
            pixels[(y * w + x) * 4..][..4].copy_from_slice(&[v, v, v, 255]);
        }
    }
    Cursor {
        buffer: MemoryRenderBuffer::from_slice(&pixels, Fourcc::Abgr8888, (w as i32, h as i32), 1, Transform::Normal, None),
        hotspot: (0, 0).into(),
    }
}

/// The pointer, drawn on top of everything; `pos` is relative to the output.
pub fn cursor_elements(
    status: &CursorImageStatus,
    cursor: &Cursor,
    renderer: &mut GlesRenderer,
    pos: Point<f64, Logical>,
    scale: Scale<f64>,
) -> Vec<SceneElement> {
    match status {
        CursorImageStatus::Hidden => Vec::new(),
        CursorImageStatus::Surface(surface) if surface.alive() => {
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map(|d| d.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            let loc = (pos - hotspot.to_f64()).to_physical_precise_round(scale);
            render_elements_from_surface_tree(renderer, surface, loc, scale, 1.0, Kind::Cursor)
        }
        _ => {
            let loc = (pos - cursor.hotspot.to_f64()).to_physical(scale);
            match MemoryRenderBufferRenderElement::from_buffer(renderer, loc, &cursor.buffer, None, None, None, Kind::Cursor) {
                Ok(e) => vec![e.into()],
                Err(e) => {
                    warn!("cannot draw the cursor: {e}");
                    Vec::new()
                }
            }
        }
    }
}

/// A client cursor surface needs frame callbacks too.
pub fn send_cursor_frame(status: &CursorImageStatus, output: &Output, time: Duration) {
    if let CursorImageStatus::Surface(surface) = status
        && surface.alive()
    {
        send_frames_surface_tree(surface, output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
    }
}
