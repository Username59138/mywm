//! The last frame of a closed window, so the config can animate it away.
//!
//! Some clients (foot, for one) attach a null buffer before they destroy their
//! window. The snapshot is taken in the commit handler before that commit
//! reaches the renderer state, so it still holds the last real frame.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Color32F, Frame, Offscreen, Renderer,
            element::{
                Element, Kind, RenderElement,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
                texture::TextureBuffer,
            },
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::Window,
    wayland::seat::WaylandFocus,
    utils::{Logical, Point, Rectangle, Size, Transform},
};

pub struct Snapshot {
    pub buffer: TextureBuffer<GlesTexture>,
    pub size: Size<i32, Logical>,
    /// where the texture starts relative to the window geometry origin
    pub offset: Point<i32, Logical>,
}

pub fn take(renderer: &mut GlesRenderer, window: &Window) -> Option<Snapshot> {
    let surface = window.wl_surface()?;
    let bbox = window.bbox();
    if bbox.size.w <= 0 || bbox.size.h <= 0 {
        return None;
    }
    let geometry = window.geometry();
    let scale = 1.0;
    let size = bbox.size.to_physical(1);

    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = render_elements_from_surface_tree(
        renderer,
        &surface,
        Point::<i32, Logical>::from((-bbox.loc.x, -bbox.loc.y)).to_physical(1),
        scale,
        1.0,
        Kind::Unspecified,
    );
    if elements.is_empty() {
        return None;
    }

    let mut texture: GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, size.to_logical(1).to_buffer(1, Transform::Normal))
        .ok()?;
    {
        let mut target = renderer.bind(&mut texture).ok()?;
        let mut frame = renderer.render(&mut target, size, Transform::Normal).ok()?;
        let full = Rectangle::from_size(size);
        frame.clear(Color32F::TRANSPARENT, &[full]).ok()?;
        // elements come front to back
        for e in elements.iter().rev() {
            let dst = e.geometry(scale.into());
            let damage = [Rectangle::from_size(dst.size)];
            e.draw(&mut frame, e.src(), dst, &damage, &[]).ok()?;
        }
        // the texture must be complete before it is sampled
        let _ = frame.finish().ok()?.wait();
    }

    Some(Snapshot {
        buffer: TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None),
        size: bbox.size,
        offset: bbox.loc - geometry.loc,
    })
}
