//! Input: every key press and button press goes to the config first. A
//! handler that returns true consumes it; otherwise it reaches the client.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event as _, InputBackend, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::{Keycode, Keysym},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::state::{Event, State};

const MOD_SHIFT: u32 = 1;
const MOD_CTRL: u32 = 2;
const MOD_ALT: u32 = 4;
const MOD_SUPER: u32 = 8;

impl State {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => self.on_key(event.key_code(), event.state(), event.time_msec()),
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(geo) = self.shared.borrow().output_geometry() else { return };
                let pos = event.position_transformed(geo.size) + geo.loc.to_f64();
                self.pointer_motion(pos, event.time_msec());
            }
            InputEvent::PointerButton { event, .. } => {
                self.on_button(event.button_code(), event.state(), event.time_msec())
            }
            InputEvent::PointerAxis { event, .. } => self.on_axis::<I>(&event),
            _ => {}
        }
    }

    fn on_key(&mut self, code: Keycode, state: KeyState, time: u32) {
        let keyboard = self.seat.get_keyboard().unwrap();
        let ((sym, mods), mods_changed) = keyboard.input_intercept::<(u32, u32), _>(self, code, state, |_, m, h| {
            // the key as it is in the first latin layout, without modifiers:
            // binds keep working with the Russian layout and with Shift held
            let sym = h.raw_latin_sym_or_raw_current_sym().unwrap_or_else(|| h.modified_sym());
            let mut bits = 0;
            if m.shift {
                bits |= MOD_SHIFT;
            }
            if m.ctrl {
                bits |= MOD_CTRL;
            }
            if m.alt {
                bits |= MOD_ALT;
            }
            if m.logo {
                bits |= MOD_SUPER;
            }
            (sym.raw(), bits)
        });
        let pressed = state == KeyState::Pressed;

        // Emergency exit that no config can take away.
        if pressed && mods & (MOD_CTRL | MOD_ALT) == (MOD_CTRL | MOD_ALT) && sym == Keysym::BackSpace.raw() {
            self.loop_signal.stop();
            return;
        }

        let raw = code.raw();
        let event = Event::Key { key: sym, mods, pressed };
        let consumed = if !pressed && self.suppressed_keys.remove(&raw) {
            self.emit(event);
            true
        } else {
            let consumed = self.emit(event);
            if consumed && pressed {
                self.suppressed_keys.insert(raw);
            }
            consumed
        };

        if !consumed {
            keyboard.input_forward(self, code, state, SERIAL_COUNTER.next_serial(), time, mods_changed);
        }
    }

    pub fn pointer_motion(&mut self, pos: Point<f64, Logical>, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let under = self.shared.borrow().surface_under(pos);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
    }

    /// The scene may have moved under a still pointer.
    pub fn refresh_pointer_focus(&mut self) {
        let pointer = self.seat.get_pointer().unwrap();
        if pointer.is_grabbed() {
            return;
        }
        let pos = pointer.current_location();
        let under = self.shared.borrow().surface_under(pos);
        if under.as_ref().map(|(s, _)| s) != pointer.current_focus().as_ref() {
            let time = self.start_time.elapsed().as_millis() as u32;
            self.pointer_motion(pos, time);
        }
    }

    fn on_button(&mut self, button: u32, state: ButtonState, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let pressed = state == ButtonState::Pressed;
        let pos = pointer.current_location();

        let event = Event::Button { x: pos.x, y: pos.y, button, pressed };
        let consumed = if !pressed && self.suppressed_buttons.remove(&button) {
            self.emit(event);
            true
        } else {
            let consumed = self.emit(event);
            if consumed && pressed {
                self.suppressed_buttons.insert(button);
            }
            consumed
        };
        if consumed {
            return;
        }

        pointer.button(
            self,
            &ButtonEvent {
                button,
                state,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
    }

    fn on_axis<I: InputBackend>(&mut self, event: &I::PointerAxisEvent) {
        let source = event.source();
        let amount = |axis| {
            event
                .amount(axis)
                .unwrap_or_else(|| event.amount_v120(axis).unwrap_or(0.0) * 15.0 / 120.0)
        };

        let mut frame = AxisFrame::new(event.time_msec()).source(source);
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let value = amount(axis);
            if value != 0.0 {
                frame = frame.value(axis, value);
                if let Some(discrete) = event.amount_v120(axis) {
                    frame = frame.v120(axis, discrete as i32);
                }
            }
            if source == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                frame = frame.stop(axis);
            }
        }

        let pointer = self.seat.get_pointer().unwrap();
        pointer.axis(self, frame);
        pointer.frame(self);
    }
}
