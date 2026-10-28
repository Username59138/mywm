use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::OsString,
    rc::Rc,
    sync::Arc,
    time::Instant,
};

use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer},
        winit::WinitGraphicsBackend,
    },
    desktop::{PopupManager, Window, WindowSurfaceType},
    input::{Seat, SeatState, keyboard::XkbConfig},
    output::Output,
    reexports::{
        calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId as WlClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::{ToplevelSurface, XdgShellState, decoration::XdgDecorationState},
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};
use tracing::{error, info, warn};

use crate::{
    lua::{self, LuaRuntime},
    scene::{CORE_OWNER, ClientId, NodeId, Scene},
    snapshot::Snapshot,
};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Stage {
    /// created, the client has not committed its initial state yet
    Pending,
    /// `client.new` was sent; waiting for the first buffer
    New,
    /// `client.ready` was sent; the window has content
    Ready,
    /// the client destroyed the window; only the snapshot is left
    Closed,
}

pub struct ClientEntry {
    pub window: Window,
    pub node: NodeId,
    pub stage: Stage,
    pub snapshot: Option<Snapshot>,
    /// decoration mode chosen by the config, if any
    pub decoration: Option<DecorationMode>,
    // cached, so they stay readable after the surface is gone
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub size: Size<i32, Logical>,
}

pub struct OutputInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
}

pub struct KeyboardSettings {
    pub layout: Option<String>,
    pub variant: Option<String>,
    pub options: Option<String>,
    pub model: Option<String>,
    pub repeat_rate: Option<i32>,
    pub repeat_delay: Option<i32>,
}

/// Requests from Lua that need the whole compositor state; they run right
/// after the Lua call that queued them returns.
pub enum Command {
    Focus(Option<ClientId>),
    Keyboard(KeyboardSettings),
    Reload,
    Quit,
}

pub enum Event {
    ClientNew(ClientId, NodeId),
    ClientReady(ClientId),
    ClientClosed(ClientId),
    ClientResize(ClientId),
    OutputChange,
    Key { key: u32, mods: u32, pressed: bool },
    Button { x: f64, y: f64, button: u32, pressed: bool },
}

/// Everything Lua closures may touch.
pub struct Shared {
    pub scene: Scene,
    pub clients: HashMap<ClientId, ClientEntry>,
    pub outputs: Vec<OutputInfo>,
    pub commands: Vec<Command>,
    pub start: Instant,
    pub next_client: ClientId,
}

impl Shared {
    pub fn client_by_surface(&self, surface: &WlSurface) -> Option<ClientId> {
        self.clients
            .iter()
            .find(|(_, e)| e.window.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .map(|(id, _)| *id)
    }

    pub fn live_toplevel(&self, id: ClientId) -> Option<&ToplevelSurface> {
        self.clients
            .get(&id)
            .filter(|e| e.stage != Stage::Closed)
            .and_then(|e| e.window.toplevel())
            .filter(|t| t.alive())
    }

    /// The topmost client surface under a point, and the surface's origin.
    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        for (id, x, y) in self.scene.input_clients() {
            let Some(e) = self.clients.get(&id) else { continue };
            if e.stage != Stage::Ready || e.snapshot.is_some() {
                continue;
            }
            let geo = e.window.geometry();
            let origin = Point::<f64, Logical>::from((x - geo.loc.x as f64, y - geo.loc.y as f64));
            if let Some((surface, loc)) = e.window.surface_under(pos - origin, WindowSurfaceType::ALL) {
                return Some((surface, loc.to_f64() + origin));
            }
        }
        None
    }

    pub fn client_at(&self, pos: Point<f64, Logical>) -> Option<ClientId> {
        for (id, x, y) in self.scene.input_clients() {
            let Some(e) = self.clients.get(&id) else { continue };
            if e.stage != Stage::Ready || e.snapshot.is_some() {
                continue;
            }
            let geo = e.window.geometry();
            let origin = Point::<f64, Logical>::from((x - geo.loc.x as f64, y - geo.loc.y as f64));
            if e.window.surface_under(pos - origin, WindowSurfaceType::ALL).is_some() {
                return Some(id);
            }
        }
        None
    }

    pub fn output_geometry(&self) -> Option<Rectangle<i32, Logical>> {
        self.outputs
            .first()
            .map(|o| Rectangle::new((o.x, o.y).into(), (o.width, o.height).into()))
    }

    /// Free closed clients that nothing shows any more.
    pub fn collect_closed(&mut self) {
        let dead: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, e)| e.stage == Stage::Closed && !self.scene.is_attached(e.node))
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            if let Some(e) = self.clients.remove(&id) {
                self.scene.remove_client_node(e.node);
            }
        }
    }
}

pub fn decoration_mode(name: &str) -> DecorationMode {
    match name {
        "server" => DecorationMode::ServerSide,
        _ => DecorationMode::ClientSide,
    }
}

pub fn spawn(argv: &[String]) -> Result<(), String> {
    let (cmd, args) = argv.split_first().ok_or("empty command")?;
    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("{cmd}: {e}"))?;
    // reap it when it exits
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub struct State {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    pub loop_handle: LoopHandle<'static, State>,
    pub nested: bool,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    _xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    _output_manager_state: OutputManagerState,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,
    pub seat: Seat<Self>,

    pub backend: Option<WinitGraphicsBackend<GlesRenderer>>,
    pub output: Option<Output>,
    pub damage_tracker: Option<OutputDamageTracker>,

    pub shared: Rc<RefCell<Shared>>,
    pub lua: Option<LuaRuntime>,
    pub lua_gen: u32,
    pub keyboard_focus: Option<ClientId>,
    /// presses the config consumed; their releases are swallowed too
    pub suppressed_keys: HashSet<u32>,
    pub suppressed_buttons: HashSet<u32>,
    repeat_info: (i32, i32),
}

impl State {
    pub fn new(event_loop: &mut EventLoop<'static, State>, display: Display<State>, nested: bool) -> Self {
        let start_time = Instant::now();
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);

        let repeat_info = (25, 600);
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "seat0");
        seat.add_keyboard(Default::default(), repeat_info.1, repeat_info.0)
            .expect("the default keymap must compile");
        seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop);

        Self {
            start_time,
            socket_name,
            display_handle: dh,
            loop_signal: event_loop.get_signal(),
            loop_handle: event_loop.handle(),
            nested,
            compositor_state,
            xdg_shell_state,
            _xdg_decoration_state: xdg_decoration_state,
            shm_state,
            _output_manager_state: output_manager_state,
            seat_state,
            data_device_state,
            popups: PopupManager::default(),
            seat,
            backend: None,
            output: None,
            damage_tracker: None,
            shared: Rc::new(RefCell::new(Shared {
                scene: Scene::new(),
                clients: HashMap::new(),
                outputs: Vec::new(),
                commands: Vec::new(),
                start: start_time,
                next_client: 1,
            })),
            lua: None,
            lua_gen: CORE_OWNER,
            keyboard_focus: None,
            suppressed_keys: HashSet::new(),
            suppressed_buttons: HashSet::new(),
            repeat_info,
        }
    }

    fn init_wayland_listener(display: Display<State>, event_loop: &mut EventLoop<'static, State>) -> OsString {
        let listening_socket = ListeningSocketSource::new_auto().expect("cannot create a wayland socket");
        let socket_name = listening_socket.socket_name().to_os_string();
        let handle = event_loop.handle();

        handle
            .insert_source(listening_socket, |stream, _, state| {
                if let Err(e) = state
                    .display_handle
                    .insert_client(stream, Arc::new(ClientState::default()))
                {
                    warn!("cannot add a wayland client: {e}");
                }
            })
            .expect("cannot listen on the wayland socket");

        handle
            .insert_source(Generic::new(display, Interest::READ, Mode::Level), |_, display, state| {
                // Safety: the display is never dropped while the loop runs
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                Ok(PostAction::Continue)
            })
            .expect("cannot add the display to the event loop");

        socket_name
    }

    pub fn update_outputs(&mut self) {
        let mut outputs = Vec::new();
        if let Some(o) = &self.output
            && let Some(mode) = o.current_mode()
        {
            let scale = o.current_scale().fractional_scale();
            let size = mode.size.to_f64().to_logical(scale);
            let loc = o.current_location();
            outputs.push(OutputInfo {
                name: o.name(),
                x: loc.x,
                y: loc.y,
                width: size.w.round() as i32,
                height: size.h.round() as i32,
                scale,
            });
        }
        self.shared.borrow_mut().outputs = outputs;
    }

    /// Send an event to the config and run the commands it queued.
    pub fn emit(&mut self, ev: Event) -> bool {
        let consumed = match &self.lua {
            Some(rt) => rt.dispatch(&ev),
            None => {
                self.emergency(&ev);
                false
            }
        };
        self.process_commands();
        consumed
    }

    /// Without any working config: show every window at the top left and
    /// focus it, so the session stays usable. Ctrl+Alt+Backspace quits.
    fn emergency(&mut self, ev: &Event) {
        if let Event::ClientReady(id) = *ev {
            let mut sh = self.shared.borrow_mut();
            let node = sh.clients[&id].node;
            let root = sh.scene.root;
            let _ = sh.scene.add_child(root, node);
            sh.commands.push(Command::Focus(Some(id)));
        }
    }

    pub fn process_commands(&mut self) {
        loop {
            let commands = std::mem::take(&mut self.shared.borrow_mut().commands);
            if commands.is_empty() {
                break;
            }
            for cmd in commands {
                match cmd {
                    Command::Focus(id) => self.set_focus(id),
                    Command::Keyboard(k) => self.apply_keyboard(k),
                    Command::Reload => match self.reload() {
                        Ok(src) => info!("config reloaded ({src})"),
                        Err(e) => error!("config reload failed, keeping the old one:\n{e}"),
                    },
                    Command::Quit => self.loop_signal.stop(),
                }
            }
        }
    }

    pub fn set_focus(&mut self, id: Option<ClientId>) {
        let surface = id.and_then(|id| {
            let sh = self.shared.borrow();
            sh.live_toplevel(id).map(|t| t.wl_surface().clone())
        });
        self.keyboard_focus = if surface.is_some() { id } else { None };
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
    }

    fn apply_keyboard(&mut self, k: KeyboardSettings) {
        let keyboard = self.seat.get_keyboard().unwrap();
        if k.layout.is_some() || k.variant.is_some() || k.options.is_some() || k.model.is_some() {
            let config = XkbConfig {
                rules: "",
                model: k.model.as_deref().unwrap_or(""),
                layout: k.layout.as_deref().unwrap_or(""),
                variant: k.variant.as_deref().unwrap_or(""),
                options: k.options.clone(),
            };
            if let Err(e) = keyboard.set_xkb_config(self, config) {
                error!("wm.input.keyboard: invalid keyboard settings: {e:?}");
            }
        }
        if k.repeat_rate.is_some() || k.repeat_delay.is_some() {
            self.repeat_info = (
                k.repeat_rate.unwrap_or(self.repeat_info.0),
                k.repeat_delay.unwrap_or(self.repeat_info.1),
            );
            keyboard.change_repeat_info(self.repeat_info.0, self.repeat_info.1);
        }
    }

    /// Load the config at startup, falling back through the candidates.
    pub fn load_initial_config(&mut self) {
        match self.load_config(lua::candidates()) {
            Ok(src) => info!("config loaded ({src})"),
            Err(e) => error!("no config could be loaded, running in emergency mode:\n{e}"),
        }
    }

    /// Reload the highest-priority config. If it fails, the running config
    /// stays as it was.
    pub fn reload(&mut self) -> Result<String, String> {
        let first = lua::candidates().into_iter().take(1).collect();
        self.load_config(first)
    }

    fn load_config(&mut self, sources: Vec<lua::ConfigSource>) -> Result<String, String> {
        let generation = self.lua_gen + 1;
        let mut errors = Vec::new();
        for src in sources {
            match LuaRuntime::new(self.shared.clone(), generation, &src, self.nested) {
                Ok(rt) => {
                    self.install_runtime(rt, generation);
                    return Ok(src.describe());
                }
                Err(e) => {
                    self.discard_generation(generation);
                    errors.push(format!("{}: {e}", src.describe()));
                }
            }
        }
        Err(errors.join("\n"))
    }

    /// Throw away what a config that failed to load created.
    fn discard_generation(&mut self, generation: u32) {
        let mut sh = self.shared.borrow_mut();
        let Shared { scene, clients, commands, .. } = &mut *sh;
        let freed = scene.destroy_owned_by(generation, &|c| clients.get(&c).is_some_and(|e| e.stage != Stage::Closed));
        for c in freed {
            clients.remove(&c);
        }
        commands.clear();
    }

    fn install_runtime(&mut self, rt: LuaRuntime, generation: u32) {
        let old = self.lua_gen;
        self.lua = None;
        {
            let mut sh = self.shared.borrow_mut();
            let Shared { scene, clients, .. } = &mut *sh;
            if old != CORE_OWNER {
                let freed =
                    scene.destroy_owned_by(old, &|c| clients.get(&c).is_some_and(|e| e.stage != Stage::Closed));
                for c in freed {
                    clients.remove(&c);
                }
            }
            // windows that were fading out belonged to the old config
            let closed: Vec<ClientId> = clients
                .iter()
                .filter(|(_, e)| e.stage == Stage::Closed)
                .map(|(id, _)| *id)
                .collect();
            for id in closed {
                if let Some(e) = clients.remove(&id) {
                    scene.remove_client_node(e.node);
                }
            }
        }
        self.lua = Some(rt);
        self.lua_gen = generation;

        // the new config adopts the windows that are already open
        let mut existing: Vec<(ClientId, NodeId, Stage)> = self
            .shared
            .borrow()
            .clients
            .iter()
            .map(|(id, e)| (*id, e.node, e.stage))
            .collect();
        existing.sort_unstable_by_key(|(id, _, _)| *id);
        for (id, node, stage) in existing {
            if stage >= Stage::New {
                self.emit(Event::ClientNew(id, node));
            }
            if stage >= Stage::Ready {
                self.emit(Event::ClientReady(id));
            }
        }
        self.process_commands();
    }

    pub fn eval(&mut self, code: &str) -> Result<String, String> {
        let result = match &self.lua {
            Some(rt) => rt.eval(code),
            None => Err("no config is loaded (emergency mode)".into()),
        };
        self.process_commands();
        result
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: WlClientId) {}
    fn disconnected(&self, _client_id: WlClientId, _reason: DisconnectReason) {}
}
