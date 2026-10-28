//! Input: every key press, button press and scroll goes to the config
//! first. A handler that returns true consumes it; otherwise it reaches the
//! client. Pointer motion is reported too, but always reaches the client.

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event as _, InputBackend, InputEvent, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{Keycode, Keysym, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::state::{Event, State};

const MOD_SHIFT: u32 = 1;
const MOD_CTRL: u32 = 2;
const MOD_ALT: u32 = 4;
const MOD_SUPER: u32 = 8;

/// XF86Switch_VT_1; Ctrl+Alt+F1 produces it with the default keymap
const KEY_SWITCH_VT_1: u32 = 0x1008_fe01;

fn mod_bits(m: &ModifiersState) -> u32 {
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
    bits
}

impl State {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => self.on_key(event.key_code(), event.state(), event.time_msec()),
            // a real mouse (libinput) reports movement, not positions
            InputEvent::PointerMotion { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                let pos = self.clamp_to_outputs(pointer.current_location() + event.delta());
                self.on_motion(pos, event.delta(), event.time_msec());
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(geo) = self.shared.borrow().output_geometry() else { return };
                let pos = event.position_transformed(geo.size) + geo.loc.to_f64();
                // the first position is where the pointer entered, not a move
                let delta = if self.pointer_seen {
                    pos - self.seat.get_pointer().unwrap().current_location()
                } else {
                    Point::default()
                };
                self.pointer_seen = true;
                self.on_motion(pos, delta, event.time_msec());
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
        let ((sym, mods, modified), mods_changed) = keyboard.input_intercept::<(u32, u32, u32), _>(self, code, state, |_, m, h| {
            // the key as it is in the first latin layout, without modifiers:
            // binds keep working with the Russian layout and with Shift held
            let sym = h.raw_latin_sym_or_raw_current_sym().unwrap_or_else(|| h.modified_sym());
            (sym.raw(), mod_bits(m), h.modified_sym().raw())
        });
        let pressed = state == KeyState::Pressed;

        // Emergency exit that no config can take away.
        if pressed && mods & (MOD_CTRL | MOD_ALT) == (MOD_CTRL | MOD_ALT) && sym == Keysym::BackSpace.raw() {
            self.loop_signal.stop();
            return;
        }

        // VT switching belongs to the session, not to the config
        if pressed
            && (KEY_SWITCH_VT_1..KEY_SWITCH_VT_1 + 12).contains(&modified)
            && let Some(udev) = self.udev.as_mut()
        {
            udev.change_vt((modified - KEY_SWITCH_VT_1 + 1) as i32);
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

    /// The modifiers held right now, as wm.mod bits.
    fn mods(&self) -> u32 {
        mod_bits(&self.seat.get_keyboard().unwrap().modifier_state())
    }

    fn wants(&self, event: &str) -> bool {
        self.lua.as_ref().is_some_and(|rt| rt.wants(event))
    }

    fn clamp_to_outputs(&self, mut pos: Point<f64, Logical>) -> Point<f64, Logical> {
        if let Some(b) = self.shared.borrow().outputs_bbox() {
            pos.x = pos.x.clamp(b.loc.x as f64, (b.loc.x + b.size.w) as f64 - 1.0);
            pos.y = pos.y.clamp(b.loc.y as f64, (b.loc.y + b.size.h) as f64 - 1.0);
        }
        pos
    }

    /// The pointer moves first, so a handler sees the new position and may
    /// warp it elsewhere.
    fn on_motion(&mut self, pos: Point<f64, Logical>, delta: Point<f64, Logical>, time: u32) {
        self.pointer_motion(pos, time);
        if self.wants("input.motion") {
            let mods = self.mods();
            self.emit(Event::Motion { x: pos.x, y: pos.y, dx: delta.x, dy: delta.y, mods });
        }
    }

    /// wm.input.warp: put the pointer somewhere (kept on the outputs).
    pub fn warp_pointer(&mut self, x: f64, y: f64) {
        let pos = self.clamp_to_outputs((x, y).into());
        let time = self.start_time.elapsed().as_millis() as u32;
        self.pointer_motion(pos, time);
    }

    pub fn pointer_motion(&mut self, pos: Point<f64, Logical>, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let under = {
            let mut sh = self.shared.borrow_mut();
            sh.pointer = (pos.x, pos.y);
            sh.surface_under(pos)
        };
        let hovered = self.shared.borrow().client_at(pos);
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

        if hovered != self.hovered {
            let old = std::mem::replace(&mut self.hovered, hovered);
            if let Some(id) = old {
                self.emit(Event::ClientHover(id, false));
            }
            if let Some(id) = hovered {
                self.emit(Event::ClientHover(id, true));
            }
        }
    }

    /// A press on a notification dismisses it and goes nowhere else.
    fn click_note(&mut self, pos: Point<f64, Logical>) -> bool {
        let mut sh = self.shared.borrow_mut();
        let corners: Vec<Point<f64, Logical>> = sh
            .outputs
            .iter()
            .filter(|o| {
                pos.x >= o.x as f64 && pos.x < (o.x + o.width) as f64 && pos.y >= o.y as f64 && pos.y < (o.y + o.height) as f64
            })
            .map(|o| Point::from(((o.x + o.width) as f64, o.y as f64)))
            .collect();
        corners.into_iter().any(|c| sh.notes.click(pos - c))
    }

    /// The scene may have moved under a still pointer.
    pub fn refresh_pointer_focus(&mut self) {
        let pointer = self.seat.get_pointer().unwrap();
        if pointer.is_grabbed() {
            return;
        }
        let pos = pointer.current_location();
        let under = self.shared.borrow().surface_under(pos);
        let hovered = self.shared.borrow().client_at(pos);
        if under.as_ref().map(|(s, _)| s) != pointer.current_focus().as_ref() || hovered != self.hovered {
            let time = self.start_time.elapsed().as_millis() as u32;
            self.pointer_motion(pos, time);
        }
    }

    fn on_button(&mut self, button: u32, state: ButtonState, time: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        let pressed = state == ButtonState::Pressed;
        let pos = pointer.current_location();

        if pressed && self.click_note(pos) {
            self.suppressed_buttons.insert(button);
            return;
        }
        let event = Event::Button { x: pos.x, y: pos.y, button, pressed, mods: self.mods() };
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
        let (dx, dy) = (amount(Axis::Horizontal), amount(Axis::Vertical));
        if (dx != 0.0 || dy != 0.0) && self.wants("input.scroll") {
            let pos = pointer.current_location();
            let wheel = source == AxisSource::Wheel || source == AxisSource::WheelTilt;
            let event = Event::Scroll { x: pos.x, y: pos.y, dx, dy, wheel, mods: self.mods() };
            if self.emit(event) {
                return;
            }
        }
        pointer.axis(self, frame);
        pointer.frame(self);
    }
}
