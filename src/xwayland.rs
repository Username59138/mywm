//! X11 apps through Xwayland.
//!
//! Normal X11 windows become clients like Wayland ones: the config gets
//! client.new / ready / closed for them (with `c.x11 = true`). Override-
//! redirect windows (menus, tooltips, dropdowns) place themselves; the core
//! draws them on top and the config never sees them.

use std::{os::fd::OwnedFd, process::Stdio};

use smithay::{
    desktop::Window,
    backend::renderer::utils::with_renderer_surface_state,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle},
    wayland::{
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata, request_primary_client_selection,
                set_primary_selection,
            },
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmId},
    },
};
use tracing::{error, info, warn};

use crate::{
    focus::FocusTarget,
    scene::{CORE_OWNER, NodeKind},
    state::{ClientEntry, Event, Request, Stage, State},
};

/// Start Xwayland; returns the display number for $DISPLAY. The X11 WM
/// attaches once Xwayland is ready.
pub fn start(state: &mut State) -> Result<u32, String> {
    let (xwayland, client) = XWayland::spawn(
        &state.display_handle,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| (),
    )
    .map_err(|e| format!("cannot start Xwayland: {e}"))?;
    let number = xwayland.display_number();

    state
        .loop_handle
        .insert_source(xwayland, move |event, _, state| match event {
            XWaylandEvent::Ready { x11_socket, .. } => {
                match X11Wm::start_wm(state.loop_handle.clone(), x11_socket, client.clone()) {
                    Ok(wm) => {
                        state.xwm = Some(wm);
                        info!("Xwayland is ready on :{number}");
                    }
                    Err(e) => error!("cannot start the X11 window manager: {e}"),
                }
            }
            XWaylandEvent::Error => warn!("Xwayland exited during startup"),
        })
        .map_err(|e| e.to_string())?;
    Ok(number)
}

impl State {
    /// A normal X11 window asked to be shown: it becomes a client.
    fn add_x11_client(&mut self, window: X11Surface) {
        let (id, node) = {
            let mut sh = self.shared.borrow_mut();
            let id = sh.next_client;
            sh.next_client += 1;
            let node = sh.scene.create(NodeKind::Client(id), CORE_OWNER);
            sh.clients.insert(
                id,
                ClientEntry {
                    title: Some(window.title()),
                    app_id: Some(window.class()),
                    size: window.geometry().size,
                    window: Window::new_x11_window(window),
                    node,
                    stage: Stage::New,
                    snapshot: None,
                    decoration: None,
                    configured: false,
                    fullscreen: false,
                    maximized: false,
                    tiled: false,
                    pid: None,
                    early: Vec::new(),
                },
            );
            (id, node)
        };
        self.emit(Event::ClientNew(id, node));
    }

    /// Every frame: X11 windows that got their content become ready (the
    /// association with their wl_surface may come after the first commit),
    /// and the X server learns where each shown window is, so the menus of
    /// X11 apps open in the right place.
    pub fn x11_frame(&mut self) {
        if self.xwm.is_none() {
            return;
        }
        let mut ready = Vec::new();
        {
            let sh = self.shared.borrow();
            for (id, e) in &sh.clients {
                let Some(x) = e.window.x11_surface() else { continue };
                if e.stage == Stage::New {
                    let has_buffer = x
                        .wl_surface()
                        .and_then(|s| with_renderer_surface_state(&s, |r| r.buffer().is_some()))
                        .unwrap_or(false);
                    if has_buffer {
                        ready.push(*id);
                    }
                }
                if e.stage == Stage::Ready && sh.scene.is_attached(e.node) {
                    let (ax, ay) = sh.scene.abs_pos(e.node);
                    let loc: Point<i32, Logical> = (ax.round() as i32, ay.round() as i32).into();
                    let geo = x.geometry();
                    if geo.loc != loc {
                        let _ = x.configure(Rectangle::new(loc, geo.size));
                    }
                }
            }
        }
        for id in ready {
            if let Some(e) = self.shared.borrow_mut().clients.get_mut(&id) {
                e.stage = Stage::Ready;
                e.size = e.window.geometry().size;
            }
            self.emit(Event::ClientReady(id));
        }
    }

    fn remove_override_redirect(&mut self, window: &X11Surface) -> bool {
        let mut sh = self.shared.borrow_mut();
        let before = sh.or_windows.len();
        sh.or_windows.retain(|w| w.x11_surface() != Some(window));
        sh.or_windows.len() != before
    }

    fn x11_request(&mut self, window: &X11Surface, request: Request) {
        let id = self.shared.borrow().client_by_x11(window);
        if let Some(id) = id {
            self.emit(Event::ClientRequest(id, request));
        }
    }

    fn x11_closed(&mut self, window: &X11Surface) {
        let id = self.shared.borrow().client_by_x11(window);
        if let Some(id) = id {
            self.client_closed(id);
        }
    }
}

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, _surface: WlSurface, _window: X11Surface) {
        // takes effect on the next commit; x11_frame picks the window up
    }
}

impl XwmHandler for State {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("X11 WM event without a WM")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Err(e) = window.set_mapped(true) {
            warn!("cannot map an X11 window: {e}");
            return;
        }
        self.add_x11_client(window);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.shared.borrow_mut().or_windows.push(Window::new_x11_window(window));
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if self.remove_override_redirect(&window) {
            return;
        }
        // an X11 window that hides is gone for the config; if it comes back
        // it is a new client
        self.x11_closed(&window);
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.remove_override_redirect(&window);
        self.x11_closed(&window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Until the config sizes a window, it gets the size it asks for;
        // after that the config decides. Positions are always ours.
        let configured = {
            let sh = self.shared.borrow();
            sh.client_by_x11(&window).is_some_and(|id| sh.clients[&id].configured)
        };
        let mut geo = window.geometry();
        if !configured {
            if let Some(w) = w {
                geo.size.w = w as i32;
            }
            if let Some(h) = h {
                geo.size.h = h as i32;
            }
        }
        let _ = window.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        // override-redirect windows move themselves; their geometry is read
        // when they are drawn
    }

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        if matches!(property, WmWindowProperty::Title | WmWindowProperty::Class) {
            let id = self.shared.borrow().client_by_x11(&window);
            if let Some(id) = id {
                self.client_title(id);
            }
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_request(&window, Request::Fullscreen(true));
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_request(&window, Request::Fullscreen(false));
    }

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_request(&window, Request::Maximize(true));
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_request(&window, Request::Maximize(false));
    }

    fn minimize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_request(&window, Request::Minimize);
    }

    fn resize_request(&mut self, _xwm: XwmId, window: X11Surface, button: u32, edge: ResizeEdge) {
        let edges = match edge {
            ResizeEdge::Top => 1,
            ResizeEdge::Bottom => 2,
            ResizeEdge::Left => 4,
            ResizeEdge::TopLeft => 5,
            ResizeEdge::BottomLeft => 6,
            ResizeEdge::Right => 8,
            ResizeEdge::TopRight => 9,
            ResizeEdge::BottomRight => 10,
        };
        self.x11_request(&window, Request::Resize(button, edges));
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, button: u32) {
        self.x11_request(&window, Request::Move(button));
    }

    /// X11 apps may read the selection only while one of them has focus.
    fn allow_selection_access(&mut self, xwm: XwmId, _selection: SelectionTarget) -> bool {
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        matches!(focus, Some(FocusTarget::X11(x)) if x.xwm_id() == Some(xwm))
    }

    /// An X11 app pastes what a Wayland app copied.
    fn send_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_type: String, fd: OwnedFd) {
        let result = match selection {
            SelectionTarget::Clipboard => request_data_device_client_selection(&self.seat, mime_type, fd).map_err(|e| e.to_string()),
            SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime_type, fd).map_err(|e| e.to_string()),
        };
        if let Err(e) = result {
            warn!("cannot hand the Wayland selection to Xwayland: {e}");
        }
    }

    /// An X11 app copied something: offer it to Wayland apps.
    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        match selection {
            SelectionTarget::Clipboard => set_data_device_selection(&self.display_handle, &self.seat, mime_types, ()),
            SelectionTarget::Primary => set_primary_selection(&self.display_handle, &self.seat, mime_types, ()),
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat);
                }
            }
        }
    }
}

smithay::delegate_xwayland_shell!(State);
