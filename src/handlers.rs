//! Wayland protocol handlers. They turn client requests into the lifecycle
//! events the config sees: `client.new`, `client.ready`, `client.closed`.

use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        renderer::{
            ImportDma,
            utils::{on_commit_buffer_handler, with_renderer_surface_state},
        },
    },
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output, delegate_primary_selection,
    delegate_seat, delegate_shm, delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{PopupKind, Window, find_popup_root_surface, get_popup_toplevel_coords},
    input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus},
    reexports::{
        calloop::Interest,
        wayland_protocols::xdg::{
            decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
            shell::server::xdg_toplevel::ResizeEdge,
        },
        wayland_server::{
            Client, Resource,
            protocol::{wl_buffer, wl_output::WlOutput, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf},
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface, with_states,
        },
        drm_syncobj::{DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState},
        output::OutputHandler,
        selection::{
            SelectionHandler, SelectionSource, SelectionTarget,
            primary_selection::{PrimarySelectionHandler, PrimarySelectionState, set_primary_focus},
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData, decoration::XdgDecorationHandler,
        },
        shm::{ShmHandler, ShmState},
    },
};

use std::os::fd::OwnedFd;

use smithay::{wayland::seat::WaylandFocus, xwayland::XWaylandClientData};
use tracing::warn;

use crate::{
    focus::FocusTarget,
    scene::{CORE_OWNER, NodeKind},
    snapshot,
    state::{ClientEntry, ClientState, Event, Request, Stage, State},
};

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Xwayland is a client too, with its own client data type
        if let Some(x) = client.get_data::<XWaylandClientData>() {
            return &x.compositor_state;
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    /// Hold a commit back until the client's GPU work on its buffer is
    /// done: its explicit-sync acquire point if it set one, otherwise the
    /// implicit fence of the dmabuf. Showing it earlier shows a half-drawn
    /// or stale frame.
    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, _dh, surface| {
            let (acquire_point, dmabuf) = with_states(surface, |data| {
                let acquire = data.cached_state.get::<DrmSyncobjCachedState>().pending().acquire_point.clone();
                let dmabuf = data
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|b| match b {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        _ => None,
                    });
                (acquire, dmabuf)
            });
            let Some(dmabuf) = dmabuf else { return };
            let Some(client) = surface.client() else { return };

            if let Some(point) = acquire_point
                && let Ok((blocker, source)) = point.generate_blocker()
            {
                let client = client.clone();
                let inserted = state.loop_handle.insert_source(source, move |_, _, state| {
                    state.unblock_client(&client);
                    Ok(())
                });
                if inserted.is_ok() {
                    add_blocker(surface, blocker);
                    return;
                }
            }
            if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) {
                let inserted = state.loop_handle.insert_source(source, move |_, _, state| {
                    state.unblock_client(&client);
                    Ok(())
                });
                if inserted.is_ok() {
                    add_blocker(surface, blocker);
                }
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Must run before the renderer state sees this commit.
        self.snapshot_if_unmapping(surface);

        on_commit_buffer_handler::<Self>(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            let window = {
                let sh = self.shared.borrow();
                sh.client_by_surface(&root).map(|id| sh.clients[&id].window.clone())
            };
            if let Some(window) = window {
                window.on_commit();
            }
        }

        self.client_commit(surface);
        self.layer_commit(surface);

        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(xdg)) = self.popups.find_popup(surface)
            && !xdg.is_initial_configure_sent()
        {
            // the initial configure is always allowed
            let _ = xdg.send_configure();
        }
    }
}

impl State {
    /// A fence a client's commit waited for has signalled.
    fn unblock_client(&mut self, client: &Client) {
        let dh = self.display_handle.clone();
        self.client_compositor_state(client).blocker_cleared(self, &dh);
    }

    /// A shown window that attaches a null buffer is unmapping: keep its last
    /// frame before the renderer state drops it.
    fn snapshot_if_unmapping(&mut self, surface: &WlSurface) {
        let removed = with_states(surface, |states| {
            matches!(
                states.cached_state.get::<SurfaceAttributes>().current().buffer,
                Some(BufferAssignment::Removed)
            )
        });
        if !removed {
            return;
        }
        let window = {
            let sh = self.shared.borrow();
            let Some(id) = sh.client_by_surface(surface) else { return };
            let e = &sh.clients[&id];
            if e.stage != Stage::Ready || e.snapshot.is_some() {
                return;
            }
            (id, e.window.clone())
        };
        self.take_snapshot(window.0, &window.1);
    }

    fn take_snapshot(&mut self, id: u64, window: &Window) {
        let Some(renderer) = self.renderer() else { return };
        let snap = snapshot::take(renderer, window);
        if let Some(e) = self.shared.borrow_mut().clients.get_mut(&id) {
            e.snapshot = snap;
        }
    }

    /// A commit of a client window's main surface (Wayland or X11).
    fn client_commit(&mut self, surface: &WlSurface) {
        let (id, stage, window) = {
            let sh = self.shared.borrow();
            let Some(id) = sh.client_by_surface(surface) else { return };
            let e = &sh.clients[&id];
            (id, e.stage, e.window.clone())
        };

        let (title, app_id) = match (window.toplevel(), window.x11_surface()) {
            (_, Some(x)) => (Some(x.title()), Some(x.class())),
            _ => with_states(surface, |states| {
                let data = states.data_map.get::<XdgToplevelSurfaceData>().unwrap().lock().unwrap();
                (data.title.clone(), data.app_id.clone())
            }),
        };
        let resized = {
            let mut sh = self.shared.borrow_mut();
            let e = sh.clients.get_mut(&id).unwrap();
            e.title = title;
            e.app_id = app_id;
            let size = window.geometry().size;
            let resized = e.size != size && size.w > 0 && size.h > 0;
            if resized {
                e.size = size;
            }
            resized
        };

        // Wayland windows wait for their first configure; X11 windows are
        // announced by the X11 WM when they ask to be mapped
        if let Some(toplevel) = window.toplevel()
            && !toplevel.is_initial_configure_sent()
        {
            if stage == Stage::Pending {
                let node = {
                    let mut sh = self.shared.borrow_mut();
                    let e = sh.clients.get_mut(&id).unwrap();
                    e.stage = Stage::New;
                    e.node
                };
                // the config may pick a size and decorations for the first configure
                self.emit(Event::ClientNew(id, node));
                let early = std::mem::take(&mut self.shared.borrow_mut().clients.get_mut(&id).unwrap().early);
                for request in early {
                    self.emit(Event::ClientRequest(id, request));
                }
            }
            toplevel.send_configure();
            return;
        }

        let mapped = with_renderer_surface_state(surface, |s| s.buffer().is_some()).unwrap_or(false);
        if !mapped {
            return;
        }
        if stage == Stage::New {
            self.shared.borrow_mut().clients.get_mut(&id).unwrap().stage = Stage::Ready;
            self.emit(Event::ClientReady(id));
        } else if stage == Stage::Ready {
            // shown again after an unmap
            self.shared.borrow_mut().clients.get_mut(&id).unwrap().snapshot = None;
            if resized {
                self.emit(Event::ClientResize(id));
            }
        }
    }

    /// A client window is gone: keep its last frame and tell the config.
    pub fn client_closed(&mut self, id: u64) {
        let (stage, has_snapshot, window) = {
            let sh = self.shared.borrow();
            let Some(e) = sh.clients.get(&id) else { return };
            (e.stage, e.snapshot.is_some(), e.window.clone())
        };

        if stage == Stage::Pending {
            // never announced to the config
            let mut sh = self.shared.borrow_mut();
            if let Some(e) = sh.clients.remove(&id) {
                sh.scene.remove_client_node(e.node);
            }
            return;
        }
        if stage == Stage::Closed {
            return;
        }

        if stage == Stage::Ready && !has_snapshot {
            self.take_snapshot(id, &window);
        }
        self.shared.borrow_mut().clients.get_mut(&id).unwrap().stage = Stage::Closed;
        self.holds.remove(&id);
        if self.hovered == Some(id) {
            self.hovered = None;
        }
        if self.keyboard_focus == Some(id) {
            self.set_focus(None);
        }
        self.emit(Event::ClientClosed(id));
        self.shared.borrow_mut().collect_closed();
    }

    /// A window asks for something; the config decides what happens.
    pub fn client_request(&mut self, surface: &WlSurface, request: Request) {
        let id = {
            let mut sh = self.shared.borrow_mut();
            let Some(id) = sh.client_by_surface(surface) else { return };
            let e = sh.clients.get_mut(&id).unwrap();
            if e.stage == Stage::Pending {
                e.early.push(request);
                return;
            }
            id
        };
        self.emit(Event::ClientRequest(id, request));
    }

    /// The title or app_id of a window changed.
    pub fn client_title(&mut self, id: u64) {
        let changed = {
            let mut sh = self.shared.borrow_mut();
            let Some(e) = sh.clients.get_mut(&id) else { return };
            let (title, app_id) = match (e.window.toplevel(), e.window.x11_surface()) {
                (_, Some(x)) => (Some(x.title()), Some(x.class())),
                (Some(t), _) => with_states(t.wl_surface(), |states| {
                    let data = states.data_map.get::<XdgToplevelSurfaceData>().unwrap().lock().unwrap();
                    (data.title.clone(), data.app_id.clone())
                }),
                _ => return,
            };
            let changed = e.title != title || e.app_id != app_id;
            e.title = title;
            e.app_id = app_id;
            changed && e.stage >= Stage::New && e.stage != Stage::Closed
        };
        if changed {
            self.emit(Event::ClientTitle(id));
        }
    }

    /// The button that started the pointer grab a move or resize refers to.
    fn grab_button(&self) -> u32 {
        self.seat
            .get_pointer()
            .and_then(|p| p.grab_start_data())
            .map_or(0x110, |d| d.button)
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let sh = self.shared.borrow();
        let Some(id) = sh.client_by_surface(&root) else { return };
        let Some(output) = sh.output_geometry() else { return };
        let (x, y) = sh.scene.abs_pos(sh.clients[&id].node);

        // the target is relative to the parent window's geometry
        let mut target = output;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= (x.round() as i32, y.round() as i32).into();

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    fn apply_decoration(&mut self, toplevel: &ToplevelSurface, requested: Option<DecorationMode>) {
        let chosen = {
            let sh = self.shared.borrow();
            sh.client_by_surface(toplevel.wl_surface())
                .and_then(|id| sh.clients[&id].decoration)
        };
        let mode = chosen.or(requested).unwrap_or(DecorationMode::ClientSide);
        toplevel.with_pending_state(|s| s.decoration_mode = Some(mode));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let pid = surface
            .wl_surface()
            .client()
            .and_then(|c| c.get_credentials(&self.display_handle).ok())
            .map(|c| c.pid);
        let window = Window::new_wayland_window(surface);
        let mut sh = self.shared.borrow_mut();
        let id = sh.next_client;
        sh.next_client += 1;
        let node = sh.scene.create(NodeKind::Client(id), CORE_OWNER);
        sh.clients.insert(
            id,
            ClientEntry {
                window,
                node,
                stage: Stage::Pending,
                snapshot: None,
                decoration: None,
                title: None,
                app_id: None,
                size: (0, 0).into(),
                configured: false,
                fullscreen: false,
                maximized: false,
                tiled: false,
                pid,
                early: Vec::new(),
            },
        );
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let id = self.shared.borrow().client_by_surface(surface.wl_surface());
        if let Some(id) = id {
            self.client_closed(id);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // TODO: popup grabs
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let id = self.shared.borrow().client_by_surface(surface.wl_surface());
        if let Some(id) = id {
            self.client_title(id);
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.title_changed(surface);
    }

    // xdg-shell wants a configure in reply to these, whatever the config does
    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        self.client_request(surface.wl_surface(), Request::Fullscreen(true));
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        self.client_request(surface.wl_surface(), Request::Fullscreen(false));
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        self.client_request(surface.wl_surface(), Request::Maximize(true));
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.client_request(surface.wl_surface(), Request::Maximize(false));
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        self.client_request(surface.wl_surface(), Request::Minimize);
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        let button = self.grab_button();
        self.client_request(surface.wl_surface(), Request::Move(button));
    }

    fn resize_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial, edges: ResizeEdge) {
        // xdg's values are already top 1, bottom 2, left 4, right 8
        let button = self.grab_button();
        self.client_request(surface.wl_surface(), Request::Resize(button, u32::from(edges)));
    }
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        self.apply_decoration(&toplevel, None);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        self.apply_decoration(&toplevel, Some(mode));
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.apply_decoration(&toplevel, None);
    }
}

impl SeatHandler for State {
    type KeyboardFocus = FocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&FocusTarget>) {
        let dh = &self.display_handle;
        let client = focused
            .and_then(|f| f.wl_surface())
            .and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();

    /// A Wayland app set the clipboard: offer it to X11 apps too.
    fn new_selection(&mut self, target: SelectionTarget, source: Option<SelectionSource>, _seat: Seat<Self>) {
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(e) = xwm.new_selection(target, source.map(|s| s.mime_types()))
        {
            warn!("cannot hand the selection to Xwayland: {e}");
        }
    }

    /// A Wayland app pastes what an X11 app copied.
    fn send_selection(
        &mut self,
        target: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(e) = xwm.send_selection(target, mime_type, fd, self.loop_handle.clone())
        {
            warn!("cannot read the selection from Xwayland: {e}");
        }
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}

impl OutputHandler for State {}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf.as_mut().expect("dmabuf global without state").0
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        let imported = self
            .renderer()
            .is_some_and(|r| r.import_dmabuf(&dmabuf, None).is_ok());
        if imported {
            let _ = notifier.successful::<State>();
        } else {
            notifier.failed();
        }
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_xdg_decoration!(State);
delegate_seat!(State);
delegate_data_device!(State);
delegate_output!(State);
delegate_dmabuf!(State);
delegate_primary_selection!(State);

impl DrmSyncobjHandler for State {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.syncobj.as_mut()
    }
}
smithay::delegate_drm_syncobj!(State);
