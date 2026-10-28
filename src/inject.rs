//! Input the config sends to one window: `c:click`, `c:press` / `c:drag` /
//! `c:release`, `c:type`, `c:key`.
//! It reaches that client without moving the pointer or the keyboard focus,
//! so which window gets the user's input stays the config's decision.

use std::collections::HashMap;

use smithay::{
    backend::input::ButtonState,
    desktop::WindowSurfaceType,
    input::{
        keyboard::{KeyboardTarget, xkb},
        pointer::{ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::{
        Resource,
        protocol::{
            wl_keyboard::{KeyState as WlKeyState, WlKeyboard},
            wl_pointer::{ButtonState as WlButtonState, WlPointer},
            wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::seat::WaylandFocus,
};
use tracing::warn;

use crate::{
    scene::ClientId,
    state::{Shared, Stage, State},
};

/// One key press as the client sees it.
#[derive(Clone, Copy)]
pub struct KeyStroke {
    /// evdev code (xkb keycode - 8)
    code: u32,
    /// depressed modifiers
    mods: u32,
    layout: u32,
}

/// Which key, layout and modifiers type each keysym in the current keymap.
#[derive(Default)]
pub struct KeyTable {
    strokes: HashMap<u32, KeyStroke>,
    /// modifier masks for the wm.mod bits shift, ctrl, alt, super
    mod_masks: [u32; 4],
}

impl KeyTable {
    pub fn build(keymap: &xkb::Keymap) -> Self {
        let mask = |name: &str| match keymap.mod_get_index(name) {
            xkb::MOD_INVALID => 0,
            i => 1 << i,
        };
        let shift = mask(xkb::MOD_NAME_SHIFT);
        let level3 = mask(xkb::MOD_NAME_ISO_LEVEL3_SHIFT);
        let mod_masks = [shift, mask(xkb::MOD_NAME_CTRL), mask(xkb::MOD_NAME_ALT), mask(xkb::MOD_NAME_LOGO)];

        // the first match wins: the first layout, then the fewest modifiers
        let mut strokes = HashMap::new();
        let mut state = xkb::State::new(keymap);
        let (min, max) = (keymap.min_keycode().raw(), keymap.max_keycode().raw());
        for layout in 0..keymap.num_layouts() {
            for mods in [0, shift, level3, shift | level3] {
                state.update_mask(mods, 0, 0, 0, 0, layout);
                for code in min.max(8)..=max {
                    let sym = state.key_get_one_sym(code.into()).raw();
                    if sym != 0 {
                        strokes.entry(sym).or_insert(KeyStroke { code: code - 8, mods, layout });
                    }
                }
            }
        }
        KeyTable { strokes, mod_masks }
    }

    /// The keys that type a text; Err is the first character no key types.
    pub fn text(&self, text: &str) -> Result<Vec<KeyStroke>, char> {
        text.chars()
            .map(|ch| {
                let sym = match ch {
                    '\n' => 0xff0d, // Return
                    '\t' => 0xff09, // Tab
                    _ => xkb::utf32_to_keysym(ch as u32).raw(),
                };
                self.strokes.get(&sym).copied().ok_or(ch)
            })
            .collect()
    }

    /// A keysym (wm.key) with wm.mod bits held.
    pub fn key(&self, sym: u32, mods: u32) -> Option<KeyStroke> {
        let mut stroke = *self.strokes.get(&sym)?;
        for (bit, mask) in self.mod_masks.iter().enumerate() {
            if mods & (1 << bit) != 0 {
                stroke.mods |= mask;
            }
        }
        Some(stroke)
    }
}

/// Buttons the config holds down in a window. The window keeps the
/// surface it was pressed on until the last button is released, like an
/// implicit grab.
pub struct Hold {
    surface: WlSurface,
    /// where that surface is, in window coordinates
    origin: Point<f64, Logical>,
    buttons: Vec<u32>,
}

impl Shared {
    /// The surface of a shown window at a point in the window's own
    /// coordinates (0, 0 is the top left of its content, like c.width).
    pub fn client_surface_at(&self, id: ClientId, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let e = self.clients.get(&id).filter(|e| e.alive() && e.stage == Stage::Ready)?;
        let geo = e.window.geometry();
        let origin = Point::<f64, Logical>::from((-geo.loc.x as f64, -geo.loc.y as f64));
        e.window
            .surface_under(pos - origin, WindowSurfaceType::ALL)
            .map(|(surface, loc)| (surface, loc.to_f64() + origin))
    }
}

impl State {
    /// The keymap changed: what each key types now.
    pub fn refresh_key_table(&mut self) {
        let keyboard = self.seat.get_keyboard().unwrap();
        let table = keyboard.with_xkb_state(self, |ctx| {
            let xkb = ctx.xkb().lock().unwrap();
            // the keymap is only used inside this call
            KeyTable::build(unsafe { xkb.keymap() })
        });
        self.shared.borrow_mut().keys = table;
    }

    /// A press and release at x, y of the window. The pointer visits the
    /// window only for these events and is back where it was before the
    /// next frame.
    pub fn inject_click(&mut self, id: ClientId, x: f64, y: f64, button: u32) {
        let pointer = self.seat.get_pointer().unwrap();
        if pointer.is_grabbed() {
            warn!("client:click skipped: a mouse button is held");
            return;
        }
        let pos: Point<f64, Logical> = (x, y).into();
        let Some(focus) = self.shared.borrow().client_surface_at(id, pos) else { return };
        let real = pointer.current_location();
        let time = self.start_time.elapsed().as_millis() as u32;

        let event = MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time };
        pointer.motion(self, Some(focus), &event);
        pointer.frame(self);
        for state in [ButtonState::Pressed, ButtonState::Released] {
            let event = ButtonEvent { button, state, serial: SERIAL_COUNTER.next_serial(), time };
            pointer.button(self, &event);
            pointer.frame(self);
        }
        self.pointer_motion(real, time);
    }

    fn client_pointers(&self, surface: &WlSurface) -> Vec<WlPointer> {
        let Some(client) = surface.client() else { return Vec::new() };
        self.seat.get_pointer().unwrap().client_pointers(&client).collect()
    }

    /// Pointer events straight to the window's client, past the seat: the
    /// real pointer stays where it is and keeps working.
    fn send_pointer(&self, surface: &WlSurface, f: impl Fn(&WlPointer, u32)) {
        let serial = u32::from(SERIAL_COUNTER.next_serial());
        for p in self.client_pointers(surface) {
            f(&p, serial);
            if p.version() >= 5 {
                p.frame();
            }
        }
    }

    fn real_pointer_on(&self, surface: &WlSurface) -> bool {
        self.seat.get_pointer().unwrap().current_focus().as_ref() == Some(surface)
    }

    /// c:press: hold a button down at x, y of the window.
    pub fn inject_press(&mut self, id: ClientId, x: f64, y: f64, button: u32) {
        let time = self.start_time.elapsed().as_millis() as u32;
        if !self.holds.contains_key(&id) {
            let Some((surface, origin)) = self.shared.borrow().client_surface_at(id, (x, y).into()) else { return };
            if !self.real_pointer_on(&surface) {
                let (sx, sy) = (x - origin.x, y - origin.y);
                self.send_pointer(&surface, |p, serial| p.enter(serial, &surface, sx, sy));
            }
            self.holds.insert(id, Hold { surface, origin, buttons: Vec::new() });
        }
        let hold = &self.holds[&id];
        if hold.buttons.contains(&button) {
            return;
        }
        let (sx, sy) = (x - hold.origin.x, y - hold.origin.y);
        self.send_pointer(&hold.surface, |p, _| p.motion(time, sx, sy));
        self.send_pointer(&hold.surface, |p, serial| p.button(serial, time, button, WlButtonState::Pressed));
        self.holds.get_mut(&id).unwrap().buttons.push(button);
    }

    /// c:drag: move the held pointer to x, y of the window.
    pub fn inject_drag(&mut self, id: ClientId, x: f64, y: f64) {
        let Some(hold) = self.holds.get(&id) else { return };
        let time = self.start_time.elapsed().as_millis() as u32;
        let (sx, sy) = (x - hold.origin.x, y - hold.origin.y);
        self.send_pointer(&hold.surface, |p, _| p.motion(time, sx, sy));
    }

    /// c:release: let a held button go; after the last one the window no
    /// longer sees the config's pointer.
    pub fn inject_release(&mut self, id: ClientId, button: u32) {
        let Some(hold) = self.holds.get_mut(&id) else { return };
        let Some(i) = hold.buttons.iter().position(|b| *b == button) else { return };
        hold.buttons.remove(i);
        let time = self.start_time.elapsed().as_millis() as u32;
        let surface = hold.surface.clone();
        let done = hold.buttons.is_empty();
        self.send_pointer(&surface, |p, serial| p.button(serial, time, button, WlButtonState::Released));
        if done {
            self.holds.remove(&id);
            if !self.real_pointer_on(&surface) {
                self.send_pointer(&surface, |p, serial| p.leave(serial, &surface));
            }
        }
    }

    /// Keys for one window. If it is not the focused one, it gets the
    /// keyboard just for these keys and the focused window gets it back.
    /// The modifiers the user holds are not seen by the window.
    pub fn inject_keys(&mut self, id: ClientId, strokes: &[KeyStroke]) {
        let target = self.shared.borrow().clients.get(&id).filter(|e| e.alive()).and_then(|e| e.focus_target());
        let Some(target) = target else { return };
        let Some(client) = target.wl_surface().and_then(|s| s.client()) else { return };
        let keyboard = self.seat.get_keyboard().unwrap();
        let kbds: Vec<WlKeyboard> = keyboard.client_keyboards(&client).collect();
        if kbds.is_empty() {
            return;
        }

        let seat = self.seat.clone();
        let current = keyboard.current_focus();
        let focused = current.as_ref() == Some(&target);
        if !focused {
            if let Some(c) = &current {
                c.leave(&seat, self, SERIAL_COUNTER.next_serial());
            }
            target.enter(&seat, self, Vec::new(), SERIAL_COUNTER.next_serial());
        }

        let time = self.start_time.elapsed().as_millis() as u32;
        for s in strokes {
            let serial = u32::from(SERIAL_COUNTER.next_serial());
            for k in &kbds {
                k.modifiers(serial, s.mods, 0, 0, s.layout);
                k.key(serial, time, s.code, WlKeyState::Pressed);
                k.key(serial, time, s.code, WlKeyState::Released);
            }
        }

        let mods = keyboard.modifier_state();
        if focused {
            let m = mods.serialized;
            let serial = u32::from(SERIAL_COUNTER.next_serial());
            for k in &kbds {
                k.modifiers(serial, m.depressed, m.latched, m.locked, m.layout_effective);
            }
        } else {
            target.leave(&seat, self, SERIAL_COUNTER.next_serial());
            if let Some(c) = &current {
                c.enter(&seat, self, Vec::new(), SERIAL_COUNTER.next_serial());
                c.modifiers(&seat, self, mods, SERIAL_COUNTER.next_serial());
            }
        }
    }
}
