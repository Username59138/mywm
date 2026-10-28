//! The winit backend: MyWM runs as a window inside another compositor.

use std::time::Duration;

use smithay::{
    backend::{
        renderer::{ImportDma, damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::{
        EventLoop,
        timer::{TimeoutAction, Timer},
    },
    utils::{Logical, Point, Rectangle, Transform},
};
use tracing::warn;

use crate::{
    render::{self, CLEAR_COLOR},
    state::{Event, State},
};

/// how often to look for changes while nothing is drawn
const IDLE_FRAME: Duration = Duration::from_millis(16);

pub struct WinitData {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    output: Output,
    damage_tracker: OutputDamageTracker,
}

pub fn init(event_loop: &mut EventLoop<State>, state: &mut State) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit) = winit::init::<GlesRenderer>()?;

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

    let formats = backend.renderer().dmabuf_formats();
    state.init_dmabuf(formats);

    state.outputs = vec![output.clone()];
    state.update_outputs();
    state.winit = Some(WinitData {
        backend,
        damage_tracker: OutputDamageTracker::from_output(&output),
        output,
    });

    event_loop.handle().insert_source(winit, move |event, _, state| match event {
        WinitEvent::Resized { size, .. } => {
            if let Some(w) = &state.winit {
                w.output.change_current_state(Some(Mode { size, refresh: 60_000 }), None, None, None);
            }
            state.update_outputs();
            state.emit(Event::OutputChange);
        }
        WinitEvent::Input(event) => state.process_input_event(event),
        WinitEvent::Redraw => state.winit_render(),
        WinitEvent::CloseRequested => state.loop_signal.stop(),
        _ => {}
    })?;

    Ok(())
}

impl State {
    fn winit_render(&mut self) {
        self.pre_frame();
        if let Some(output) = self.winit.as_ref().map(|w| w.output.clone()) {
            self.emit_frame(&output);
        }
        let presented = self.winit_draw();

        // After a swap, winit sends the next Redraw once the host compositor
        // has shown the frame. Without a swap no Redraw comes, so a timer
        // looks for changes instead.
        if presented {
            if let Some(w) = &self.winit {
                w.backend.window().request_redraw();
            }
        } else {
            let timer = Timer::from_duration(IDLE_FRAME);
            let _ = self.loop_handle.insert_source(timer, |_, _, state| {
                state.winit_render();
                TimeoutAction::Drop
            });
        }
    }

    /// Render one frame; returns whether it was presented.
    fn winit_draw(&mut self) -> bool {
        let Some(w) = self.winit.as_mut() else { return false };
        let output = w.output.clone();
        let scale = output.current_scale().fractional_scale().into();
        let age = w.backend.buffer_age().unwrap_or(0);
        let mut sh = self.shared.borrow_mut();
        let width = sh.output_geometry().map_or(0, |g| g.size.w);

        let damage = {
            let (renderer, mut framebuffer) = match w.backend.bind() {
                Ok(b) => b,
                Err(e) => {
                    warn!("cannot bind the winit surface: {e}");
                    return false;
                }
            };
            // the host draws the pointer, so no cursor elements here
            let origin = Point::from((0.0, 0.0));
            let mut elements = sh.notes.elements(renderer, origin, width, scale);
            elements.extend(render::scene_elements(&sh, renderer, origin, scale));
            match w
                .damage_tracker
                .render_output(renderer, &mut framebuffer, age, &elements, CLEAR_COLOR)
            {
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
            Some(damage) => match w.backend.submit(Some(damage)) {
                Ok(()) => true,
                Err(e) => {
                    warn!("cannot present the frame: {e}");
                    false
                }
            },
            None => false,
        };

        let area: Rectangle<f64, Logical> = sh.output_geometry().map(|g| g.to_f64()).unwrap_or_default();
        render::send_frames(&sh, &output, area, self.start_time.elapsed());
        drop(sh);

        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();
        presented
    }
}
