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
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{PopupKind, Window, find_popup_root_surface, get_popup_toplevel_coords},
    input::{Seat, SeatHandler, SeatState, pointer::CursorImageStatus},
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
        wayland_server::{
            Client, Resource,
            protocol::{wl_buffer, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            get_parent, is_sync_subsurface, with_states,
        },
        output::OutputHandler,
        selection::{
            SelectionHandler,
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

use crate::{
    scene::{CORE_OWNER, NodeKind},
    snapshot,
    state::{ClientEntry, ClientState, Event, Stage, State},
};

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
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

        self.toplevel_commit(surface);

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

    fn toplevel_commit(&mut self, surface: &WlSurface) {
        let (id, stage, toplevel, window) = {
            let sh = self.shared.borrow();
            let Some(id) = sh.client_by_surface(surface) else { return };
            let e = &sh.clients[&id];
            (id, e.stage, e.window.toplevel().unwrap().clone(), e.window.clone())
        };

        let (title, app_id) = with_states(surface, |states| {
            let data = states.data_map.get::<XdgToplevelSurfaceData>().unwrap().lock().unwrap();
            (data.title.clone(), data.app_id.clone())
        });
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

        if !toplevel.is_initial_configure_sent() {
            if stage == Stage::Pending {
                let node = {
                    let mut sh = self.shared.borrow_mut();
                    let e = sh.clients.get_mut(&id).unwrap();
                    e.stage = Stage::New;
                    e.node
                };
                // the config may pick a size and decorations for the first configure
                self.emit(Event::ClientNew(id, node));
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
            },
        );
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let (id, stage, has_snapshot, window) = {
            let sh = self.shared.borrow();
            let Some(id) = sh.client_by_surface(surface.wl_surface()) else { return };
            let e = &sh.clients[&id];
            (id, e.stage, e.snapshot.is_some(), e.window.clone())
        };

        if stage == Stage::Pending {
            // never announced to the config
            let mut sh = self.shared.borrow_mut();
            if let Some(e) = sh.clients.remove(&id) {
                sh.scene.remove_client_node(e.node);
            }
            return;
        }

        if stage == Stage::Ready && !has_snapshot {
            self.take_snapshot(id, &window);
        }
        self.shared.borrow_mut().clients.get_mut(&id).unwrap().stage = Stage::Closed;
        if self.keyboard_focus == Some(id) {
            self.set_focus(None);
        }
        self.emit(Event::ClientClosed(id));
        self.shared.borrow_mut().collect_closed();
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
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();
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
