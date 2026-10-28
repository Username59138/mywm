//! The winit backend: MyWM runs as a window inside another compositor.

use std::time::{Duration, Instant};

use smithay::{
    backend::{
        renderer::{
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, Kind, render_elements, solid::SolidColorRenderElement,
                surface::WaylandSurfaceRenderElement, texture::TextureRenderElement,
            },
            gles::{GlesRenderer, GlesTexture},
        },
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::{
        EventLoop,
        timer::{TimeoutAction, Timer},
    },
    utils::{Logical, Point, Transform},
};
use tracing::warn;

use crate::{
    scene::{DrawItem, NodeKind},
    state::{Event, Stage, State},
};

const CLEAR_COLOR: [f32; 4] = [0.11, 0.11, 0.12, 1.0];
/// how often to look for changes while nothing is drawn
const IDLE_FRAME: Duration = Duration::from_millis(16);

render_elements! {
    pub SceneElement<=GlesRenderer>;
    Surface = WaylandSurfaceRenderElement<GlesRenderer>,
    Solid = SolidColorRenderElement,
    Texture = TextureRenderElement<GlesTexture>,
}

pub fn init(event_loop: &mut EventLoop<State>, state: &mut State) -> Result<(), Box<dyn std::error::Error>> {
    let (backend, winit) = winit::init::<GlesRenderer>()?;

    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };
    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "MyWM".into(),
            model: "Winit".into(),
        },
    );
    let _global = output.create_global::<State>(&state.display_handle);
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, Some((0, 0).into()));
    output.set_preferred(mode);

    state.output = Some(output.clone());
    state.backend = Some(backend);
    state.update_outputs();

    state.damage_tracker = Some(OutputDamageTracker::from_output(&output));

    event_loop.handle().insert_source(winit, move |event, _, state| match event {
        WinitEvent::Resized { size, .. } => {
            output.change_current_state(Some(Mode { size, refresh: 60_000 }), None, None, None);
            state.update_outputs();
            state.emit(Event::OutputChange);
        }
        WinitEvent::Input(event) => state.process_input_event(event),
        WinitEvent::Redraw => state.render_frame(),
        WinitEvent::CloseRequested => state.loop_signal.stop(),
        _ => {}
    })?;

    Ok(())
}

impl State {
    fn render_frame(&mut self) {
        let Some(output) = self.output.clone() else { return };
        let Some(mut damage_tracker) = self.damage_tracker.take() else { return };
        let presented = self.draw(&output, &mut damage_tracker);
        self.damage_tracker = Some(damage_tracker);

        // After a swap, winit sends the next Redraw once the host compositor
        // has shown the frame. Without a swap no Redraw comes, so a timer
        // looks for changes instead.
        if presented {
            if let Some(backend) = self.backend.as_ref() {
                backend.window().request_redraw();
            }
        } else {
            let timer = Timer::from_duration(IDLE_FRAME);
            let _ = self.loop_handle.insert_source(timer, |_, _, state| {
                state.render_frame();
                TimeoutAction::Drop
            });
        }
    }

    /// Render one frame; returns whether it was presented.
    fn draw(&mut self, output: &Output, damage_tracker: &mut OutputDamageTracker) -> bool {
        // animations first: their `done` callbacks may change the scene
        let tick = self.shared.borrow_mut().scene.tick(Instant::now());
        if let Some(rt) = &self.lua {
            rt.animations_done(&tick);
        }
        self.process_commands();
        self.shared.borrow_mut().collect_closed();
        self.refresh_pointer_focus();

        let scale = output.current_scale().fractional_scale().into();
        let Some(backend) = self.backend.as_mut() else { return false };
        let age = backend.buffer_age().unwrap_or(0);
        let sh = self.shared.borrow();
        let items = sh.scene.draw_list();

        let damage = {
            let (renderer, mut framebuffer) = match backend.bind() {
                Ok(b) => b,
                Err(e) => {
                    warn!("cannot bind the winit surface: {e}");
                    return false;
                }
            };

            // render elements go front to back
            let mut elements: Vec<SceneElement> = Vec::new();
            for item in items.iter().rev() {
                match *item {
                    DrawItem::Rect { node, x, y, alpha } => {
                        if let NodeKind::Rect { buffer, .. } = &sh.scene.nodes[node].kind {
                            let loc = Point::<f64, Logical>::from((x, y)).to_physical_precise_round(scale);
                            elements.push(
                                SolidColorRenderElement::from_buffer(buffer, loc, scale, alpha, Kind::Unspecified)
                                    .into(),
                            );
                        }
                    }
                    DrawItem::Client { client, x, y, alpha } => {
                        let Some(e) = sh.clients.get(&client) else { continue };
                        if let Some(snap) = &e.snapshot {
                            let loc = Point::<f64, Logical>::from((
                                x + snap.offset.x as f64,
                                y + snap.offset.y as f64,
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
                            let loc = Point::<f64, Logical>::from((x - geo.loc.x as f64, y - geo.loc.y as f64))
                                .to_physical_precise_round(scale);
                            elements.extend(e.window.render_elements::<SceneElement>(renderer, loc, scale, alpha));
                        }
                    }
                }
            }

            match damage_tracker.render_output(renderer, &mut framebuffer, age, &elements, CLEAR_COLOR) {
                Ok(result) => result.damage.cloned(),
                Err(e) => {
                    warn!("rendering failed: {e:?}");
                    None
                }
            }
        };

        // Nothing changed: keep the old frame (swapping would show a stale
        // back buffer).
        let presented = match &damage {
            Some(damage) => match backend.submit(Some(damage)) {
                Ok(()) => true,
                Err(e) => {
                    warn!("cannot present the frame: {e}");
                    false
                }
            },
            None => false,
        };

        // only windows that are on screen get frame callbacks; hidden ones
        // stop drawing until the config shows them again
        let time = self.start_time.elapsed();
        for item in &items {
            if let DrawItem::Client { client, .. } = *item
                && let Some(e) = sh.clients.get(&client)
                && e.stage == Stage::Ready
                && e.snapshot.is_none()
            {
                e.window.send_frame(output, time, Some(Duration::ZERO), |_, _| Some(output.clone()));
            }
        }
        drop(sh);

        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();
        presented
    }
}
