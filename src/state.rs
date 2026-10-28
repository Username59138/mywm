use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use smithay::{
    backend::{allocator::Format, renderer::gles::GlesRenderer},
    desktop::{LayerSurface, PopupManager, Window, WindowSurfaceType, layer_map_for_output},
    input::{Seat, SeatState, keyboard::XkbConfig, pointer::CursorImageStatus},
    utils::IsAlive,
    wayland::{
        seat::WaylandFocus, selection::primary_selection::PrimarySelectionState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::{X11Surface, X11Wm},
    output::Output,
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
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
        dmabuf::{DmabufGlobal, DmabufState},
        drm_syncobj::DrmSyncobjState,
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::{
            wlr_layer::{Layer as WlrLayer, WlrLayerShellState},
            xdg::{ToplevelSurface, XdgShellState, decoration::XdgDecorationState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};
use tracing::{error, info, warn};

use crate::{
    config::{self, ConfigSource},
    focus::FocusTarget,
    inject::{Hold, KeyStroke, KeyTable},
    notify::{Level, Notes},
    udev::UdevData,
    winit::WinitData,
    lua::LuaRuntime,
    scene::{CORE_OWNER, ClientId, Hit, LayerId, NodeId, Scene},
    snapshot::Snapshot,
};

/// How often autoreload looks at the config files.
const AUTORELOAD_POLL: Duration = Duration::from_millis(500);

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
    /// the config has sized this window (an X11 window may pick its own
    /// size until then)
    pub configured: bool,
    /// states the config set (c:set_fullscreen, ...)
    pub fullscreen: bool,
    pub maximized: bool,
    pub tiled: bool,
    /// of a Wayland client (X11 windows report their own)
    pub pid: Option<i32>,
    /// requests made before the config knew the window (fullscreen at
    /// startup); they follow client.new
    pub early: Vec<Request>,
}

impl ClientEntry {
    pub fn alive(&self) -> bool {
        self.stage != Stage::Closed && self.window.alive()
    }

    pub fn focus_target(&self) -> Option<FocusTarget> {
        if !self.alive() {
            return None;
        }
        match self.window.x11_surface() {
            Some(x) => Some(FocusTarget::X11(x.clone())),
            None => self.window.toplevel().map(|t| FocusTarget::Surface(t.wl_surface().clone())),
        }
    }

    pub fn configure(&mut self, width: i32, height: i32) {
        self.configured = true;
        if let Some(x) = self.window.x11_surface() {
            // X11 keeps the window's position too; it is synced every frame
            let loc = x.geometry().loc;
            let _ = x.configure(Rectangle::new(loc, (width, height).into()));
        } else if let Some(t) = self.window.toplevel() {
            t.with_pending_state(|s| s.size = Some((width, height).into()));
            if t.is_initial_configure_sent() {
                t.send_pending_configure();
            }
        }
    }

    pub fn set_state(&mut self, state: WindowState, on: bool) {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as Xdg;
        let xdg: &[Xdg] = match state {
            WindowState::Fullscreen => {
                self.fullscreen = on;
                &[Xdg::Fullscreen]
            }
            WindowState::Maximized => {
                self.maximized = on;
                &[Xdg::Maximized]
            }
            WindowState::Tiled => {
                self.tiled = on;
                &[Xdg::TiledLeft, Xdg::TiledRight, Xdg::TiledTop, Xdg::TiledBottom]
            }
        };
        if let Some(x) = self.window.x11_surface() {
            let _ = match state {
                WindowState::Fullscreen => x.set_fullscreen(on),
                WindowState::Maximized => x.set_maximized(on),
                WindowState::Tiled => Ok(()),
            };
        } else if let Some(t) = self.window.toplevel() {
            t.with_pending_state(|s| {
                for st in xdg {
                    if on {
                        s.states.set(*st);
                    } else {
                        s.states.unset(*st);
                    }
                }
            });
            if t.is_initial_configure_sent() {
                t.send_pending_configure();
            }
        }
    }

    /// The window this one belongs to (a dialog's main window).
    pub fn parent_key(&self) -> Option<ParentKey> {
        if let Some(x) = self.window.x11_surface() {
            x.is_transient_for().map(ParentKey::X11)
        } else {
            self.window.toplevel().and_then(|t| t.parent()).map(ParentKey::Surface)
        }
    }

    /// (min, max) size the app asks for; 0 = no limit.
    pub fn size_hints(&self) -> (Size<i32, Logical>, Size<i32, Logical>) {
        if let Some(x) = self.window.x11_surface() {
            (x.min_size().unwrap_or_default(), x.max_size().unwrap_or_default())
        } else if let Some(t) = self.window.toplevel() {
            smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                let mut cached = states.cached_state.get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
                let s = cached.current();
                (s.min_size, s.max_size)
            })
        } else {
            Default::default()
        }
    }

    pub fn close(&self) {
        if let Some(x) = self.window.x11_surface() {
            let _ = x.close();
        } else if let Some(t) = self.window.toplevel() {
            t.send_close();
        }
    }

    pub fn set_activated(&self, on: bool) {
        if let Some(x) = self.window.x11_surface() {
            let _ = x.set_activated(on);
        } else if let Some(t) = self.window.toplevel() {
            self.window.set_activated(on);
            if t.is_initial_configure_sent() {
                t.send_pending_configure();
            }
        }
    }
}

#[derive(Clone, Copy)]
pub enum WindowState {
    Fullscreen,
    Maximized,
    Tiled,
}

pub enum ParentKey {
    Surface(WlSurface),
    X11(u32),
}

/// Something a window asks the config for; the config decides.
pub enum Request {
    Fullscreen(bool),
    Maximize(bool),
    Minimize,
    /// an interactive move, with the button that started it
    Move(u32),
    /// an interactive resize; edges: top 1, bottom 2, left 4, right 8
    Resize(u32, u32),
}

/// A layer-shell surface: a bar, a wallpaper, a launcher, a notification.
pub struct LayerEntry {
    pub surface: LayerSurface,
    pub node: NodeId,
    pub output: String,
    /// New until it has a buffer, Ready while shown
    pub stage: Stage,
    /// where it asked to be, in global coordinates
    pub geo: Rectangle<i32, Logical>,
}

impl LayerEntry {
    pub fn layer(&self) -> WlrLayer {
        self.surface.layer()
    }

    pub fn wants_keyboard(&self) -> bool {
        self.surface.can_receive_keyboard_focus()
    }
}

pub struct OutputInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    /// logical size (physical size divided by the scale)
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    pub mode: Option<OutputMode>,
    pub modes: Vec<OutputMode>,
    pub preferred: Option<OutputMode>,
    /// what bars and panels leave free, in global coordinates
    pub usable: Rectangle<i32, Logical>,
}

/// A mode as the monitor offers it; the size is in physical pixels and
/// the refresh rate in mHz.
pub type OutputMode = smithay::output::Mode;

/// What the config asked for an output; it is kept, so a monitor that is
/// plugged in again gets it too.
#[derive(Clone, Default)]
pub struct OutputConfig {
    /// width, height, refresh in Hz (None: the highest one)
    pub mode: Option<(i32, i32, Option<f64>)>,
    pub position: Option<(i32, i32)>,
    pub scale: Option<f64>,
}

impl OutputConfig {
    fn merge(&mut self, other: OutputConfig) {
        if other.mode.is_some() {
            self.mode = other.mode;
        }
        if other.position.is_some() {
            self.position = other.position;
        }
        if other.scale.is_some() {
            self.scale = other.scale;
        }
    }
}

/// The mode of `modes` that matches a request: same size, the closest
/// refresh rate (or the highest one if none is asked for).
pub fn find_mode(modes: &[OutputMode], width: i32, height: i32, refresh: Option<f64>) -> Option<OutputMode> {
    let same_size = modes.iter().filter(|m| m.size.w == width && m.size.h == height);
    match refresh {
        Some(hz) => same_size
            .min_by(|a, b| {
                let da = (a.refresh as f64 / 1000.0 - hz).abs();
                let db = (b.refresh as f64 / 1000.0 - hz).abs();
                da.total_cmp(&db)
            })
            .copied(),
        None => same_size.max_by_key(|m| m.refresh).copied(),
    }
}

pub fn format_mode(m: &OutputMode) -> String {
    format!("{}x{}@{:.3}", m.size.w, m.size.h, m.refresh as f64 / 1000.0)
}

pub struct KeyboardSettings {
    pub layout: Option<String>,
    pub variant: Option<String>,
    pub options: Option<String>,
    pub model: Option<String>,
    pub repeat_rate: Option<i32>,
    pub repeat_delay: Option<i32>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Accel {
    Flat,
    Adaptive,
}

/// libinput settings for mice or for touchpads; None = the device default.
#[derive(Clone, Default)]
pub struct PointerSettings {
    /// -1..1
    pub speed: Option<f64>,
    pub accel: Option<Accel>,
    pub natural_scroll: Option<bool>,
    pub left_handed: Option<bool>,
    /// touchpads only
    pub tap: Option<bool>,
    /// touchpads only: disable while typing
    pub dwt: Option<bool>,
}

impl PointerSettings {
    pub fn merge(&mut self, other: PointerSettings) {
        self.speed = other.speed.or(self.speed);
        self.accel = other.accel.or(self.accel);
        self.natural_scroll = other.natural_scroll.or(self.natural_scroll);
        self.left_handed = other.left_handed.or(self.left_handed);
        self.tap = other.tap.or(self.tap);
        self.dwt = other.dwt.or(self.dwt);
    }
}

/// Requests from Lua that need the whole compositor state; they run right
/// after the Lua call that queued them returns.
pub enum Command {
    Focus(Option<ClientId>),
    Keyboard(KeyboardSettings),
    Mouse(PointerSettings),
    Touchpad(PointerSettings),
    Warp(f64, f64),
    Click { id: ClientId, x: f64, y: f64, button: u32 },
    Press { id: ClientId, x: f64, y: f64, button: u32 },
    Drag { id: ClientId, x: f64, y: f64 },
    Release { id: ClientId, button: u32 },
    Keys(ClientId, Vec<KeyStroke>),
    Autoreload(bool),
    ConfigureOutput(String, OutputConfig),
    Reload,
    Quit,
}

/// The config picked with `mywm set config` (its init.lua).
pub struct Chosen {
    pub path: PathBuf,
    /// only for this session; the saved choice stays as it was
    pub temp: bool,
}

pub enum Event {
    ClientNew(ClientId, NodeId),
    ClientReady(ClientId),
    ClientClosed(ClientId),
    ClientResize(ClientId),
    /// its title or app_id changed
    ClientTitle(ClientId),
    /// a layer surface's first commit: it said where it wants to be
    LayerNew(LayerId, NodeId),
    LayerReady(LayerId),
    /// it moved or changed size (an output change, or the app asked)
    LayerChange(LayerId),
    /// hidden; `true` if it is gone for good
    LayerClosed(LayerId, bool),
    /// the pointer entered (true) or left a window
    ClientHover(ClientId, bool),
    ClientRequest(ClientId, Request),
    /// a monitor appeared; it is not lit yet, so the config can pick its mode
    OutputAdded(String),
    OutputRemoved(String),
    OutputChange,
    /// a frame of this output is about to be drawn; dt = seconds since its
    /// previous one (only sent while the config has handlers)
    OutputFrame { output: String, dt: f64 },
    Key { key: u32, mods: u32, pressed: bool },
    Button { x: f64, y: f64, button: u32, pressed: bool, mods: u32 },
    /// the pointer moved to x, y by dx, dy (only sent while the config has
    /// handlers)
    Motion { x: f64, y: f64, dx: f64, dy: f64, mods: u32 },
    /// dy > 0 scrolls down; a wheel notch is about 15
    Scroll { x: f64, y: f64, dx: f64, dy: f64, wheel: bool, mods: u32 },
}

/// Everything Lua closures may touch.
pub struct Shared {
    pub scene: Scene,
    pub clients: HashMap<ClientId, ClientEntry>,
    pub layers: HashMap<LayerId, LayerEntry>,
    pub outputs: Vec<OutputInfo>,
    pub commands: Vec<Command>,
    pub start: Instant,
    pub next_client: ClientId,
    /// X11 override-redirect windows (menus, tooltips): they place
    /// themselves and the core draws them on top, the config never sees them
    pub or_windows: Vec<Window>,
    /// where the pointer is, for wm.input.pointer()
    pub pointer: (f64, f64),
    /// what each key types, for c:type and c:key
    pub keys: KeyTable,
    pub notes: Notes,
}

impl Shared {
    pub fn new(start: Instant) -> Self {
        Shared {
            scene: Scene::new(),
            clients: HashMap::new(),
            layers: HashMap::new(),
            outputs: Vec::new(),
            commands: Vec::new(),
            start,
            next_client: 1,
            or_windows: Vec::new(),
            pointer: (0.0, 0.0),
            keys: KeyTable::default(),
            notes: Notes::default(),
        }
    }

    pub fn client_by_surface(&self, surface: &WlSurface) -> Option<ClientId> {
        self.clients
            .iter()
            .find(|(_, e)| e.window.wl_surface().is_some_and(|s| *s == *surface))
            .map(|(id, _)| *id)
    }

    pub fn client_by_x11(&self, window: &X11Surface) -> Option<ClientId> {
        self.clients
            .iter()
            .find(|(_, e)| e.window.x11_surface() == Some(window))
            .map(|(id, _)| *id)
    }

    pub fn live_client(&mut self, id: ClientId) -> Option<&mut ClientEntry> {
        self.clients.get_mut(&id).filter(|e| e.alive())
    }

    pub fn live_toplevel(&self, id: ClientId) -> Option<&ToplevelSurface> {
        self.clients
            .get(&id)
            .filter(|e| e.stage != Stage::Closed)
            .and_then(|e| e.window.toplevel())
            .filter(|t| t.alive())
    }

    /// The topmost client surface under a point, and the surface's origin.
    /// What is under a point: X11 menus, then the scene. The hit is None
    /// for an X11 menu.
    fn pick(&self, pos: Point<f64, Logical>) -> Option<(Option<Hit>, WlSurface, Point<f64, Logical>)> {
        for w in self.or_windows.iter().rev() {
            let Some(x) = w.x11_surface() else { continue };
            let origin = x.geometry().loc.to_f64();
            if let Some((surface, loc)) = w.surface_under(pos - origin, WindowSurfaceType::ALL) {
                return Some((None, surface, loc.to_f64() + origin));
            }
        }
        for (hit, x, y) in self.scene.input_nodes() {
            let origin = Point::<f64, Logical>::from((x, y));
            match hit {
                Hit::Client(id) => {
                    let Some(e) = self.clients.get(&id) else { continue };
                    if e.stage != Stage::Ready || e.snapshot.is_some() {
                        continue;
                    }
                    let geo = e.window.geometry();
                    let origin = origin - geo.loc.to_f64();
                    if let Some((surface, loc)) = e.window.surface_under(pos - origin, WindowSurfaceType::ALL) {
                        return Some((Some(hit), surface, loc.to_f64() + origin));
                    }
                }
                Hit::Layer(id) => {
                    let Some(l) = self.layers.get(&id).filter(|l| l.stage == Stage::Ready) else { continue };
                    if let Some((surface, loc)) = l.surface.surface_under(pos - origin, WindowSurfaceType::ALL) {
                        return Some((Some(hit), surface, loc.to_f64() + origin));
                    }
                }
            }
        }
        None
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.pick(pos).map(|(_, surface, loc)| (surface, loc))
    }

    /// The window under a point; None if something else (a bar) covers it.
    pub fn client_at(&self, pos: Point<f64, Logical>) -> Option<ClientId> {
        match self.pick(pos) {
            Some((Some(Hit::Client(id)), ..)) => Some(id),
            _ => None,
        }
    }

    pub fn layer_at(&self, pos: Point<f64, Logical>) -> Option<LayerId> {
        match self.pick(pos) {
            Some((Some(Hit::Layer(id)), ..)) => Some(id),
            _ => None,
        }
    }

    pub fn layer_by_surface(&self, surface: &WlSurface) -> Option<LayerId> {
        self.layers
            .iter()
            .find(|(_, l)| l.surface.wl_surface() == surface)
            .map(|(id, _)| *id)
    }

    /// The rectangle around all outputs; the pointer stays inside it.
    pub fn outputs_bbox(&self) -> Option<Rectangle<i32, Logical>> {
        self.outputs
            .iter()
            .map(|o| Rectangle::new((o.x, o.y).into(), (o.width, o.height).into()))
            .reduce(|a, b| a.merge(b))
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

pub fn spawn(argv: &[String], env: &HashMap<String, Option<String>>) -> Result<(), String> {
    let (cmd, args) = argv.split_first().ok_or("empty command")?;
    let mut command = std::process::Command::new(cmd);
    command.args(args).stdin(std::process::Stdio::null());
    for (name, value) in env {
        match value {
            Some(v) => command.env(name, v),
            None => command.env_remove(name),
        };
    }
    let mut child = command.spawn().map_err(|e| format!("{cmd}: {e}"))?;
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
    pub primary_selection_state: PrimarySelectionState,
    pub xwayland_shell_state: XWaylandShellState,
    pub xwm: Option<X11Wm>,

    pub winit: Option<WinitData>,
    pub udev: Option<UdevData>,
    pub outputs: Vec<Output>,
    pub cursor_status: CursorImageStatus,
    pub dmabuf: Option<(DmabufState, DmabufGlobal)>,
    /// explicit sync (linux-drm-syncobj-v1), when the GPU driver supports it
    pub syncobj: Option<DrmSyncobjState>,

    pub shared: Rc<RefCell<Shared>>,
    pub lua: Option<LuaRuntime>,
    pub lua_gen: u32,
    pub keyboard_focus: Option<ClientId>,
    pub layer_shell_state: WlrLayerShellState,
    /// presses the config consumed; their releases are swallowed too
    pub suppressed_keys: HashSet<u32>,
    pub suppressed_buttons: HashSet<u32>,
    repeat_info: (i32, i32),
    /// an absolute pointer (nested) has reported a position
    pub pointer_seen: bool,
    /// the window under the pointer, for client.enter / leave
    pub hovered: Option<ClientId>,
    /// buttons the config holds down in windows (c:press)
    pub holds: HashMap<ClientId, Hold>,
    pub mouse: PointerSettings,
    pub touchpad: PointerSettings,

    pub output_configs: HashMap<String, OutputConfig>,
    /// when each output last had an output.frame, for dt
    last_frame: HashMap<String, Instant>,
    pub chosen: Option<Chosen>,
    /// the file of the running config; None for the built-in one
    pub loaded: Option<PathBuf>,
    pub autoreload: bool,
    autoreload_stamp: (Option<SystemTime>, usize),
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
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);

        let repeat_info = (25, 600);
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "seat0");
        seat.add_keyboard(Default::default(), repeat_info.1, repeat_info.0)
            .expect("the default keymap must compile");
        seat.add_pointer();

        let socket_name = Self::init_wayland_listener(display, event_loop);

        event_loop
            .handle()
            .insert_source(Timer::from_duration(AUTORELOAD_POLL), |_, _, state| {
                state.check_autoreload();
                TimeoutAction::ToDuration(AUTORELOAD_POLL)
            })
            .expect("cannot add the autoreload timer");

        let mut state = Self {
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
            primary_selection_state,
            xwayland_shell_state,
            xwm: None,
            winit: None,
            udev: None,
            outputs: Vec::new(),
            cursor_status: CursorImageStatus::default_named(),
            dmabuf: None,
            syncobj: None,
            shared: Rc::new(RefCell::new(Shared::new(start_time))),
            lua: None,
            lua_gen: CORE_OWNER,
            keyboard_focus: None,
            layer_shell_state,
            suppressed_keys: HashSet::new(),
            suppressed_buttons: HashSet::new(),
            repeat_info,
            pointer_seen: false,
            hovered: None,
            holds: HashMap::new(),
            mouse: PointerSettings::default(),
            touchpad: PointerSettings::default(),
            output_configs: HashMap::new(),
            last_frame: HashMap::new(),
            chosen: None,
            loaded: None,
            autoreload: false,
            autoreload_stamp: (None, 0),
        };
        state.refresh_key_table();
        state
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
        let outputs = self
            .outputs
            .iter()
            .filter_map(|o| {
                let mode = o.current_mode()?;
                let scale = o.current_scale().fractional_scale();
                let size = mode.size.to_f64().to_logical(scale);
                let loc = o.current_location();
                let mut modes = o.modes();
                modes.sort_by_key(|m| std::cmp::Reverse((m.size.w * m.size.h, m.refresh)));
                modes.dedup();
                let mut usable = layer_map_for_output(o).non_exclusive_zone();
                usable.loc += loc;
                Some(OutputInfo {
                    usable,
                    name: o.name(),
                    x: loc.x,
                    y: loc.y,
                    width: size.w.round() as i32,
                    height: size.h.round() as i32,
                    scale,
                    mode: Some(mode),
                    modes,
                    preferred: o.preferred_mode(),
                })
            })
            .collect();
        self.shared.borrow_mut().outputs = outputs;
    }

    /// The renderer of whichever backend runs.
    pub fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        if let Some(w) = self.winit.as_mut() {
            return Some(w.backend.renderer());
        }
        self.udev.as_mut().and_then(|u| u.renderer())
    }

    /// linux-dmabuf, so GPU clients can hand over their buffers.
    pub fn init_dmabuf(&mut self, formats: impl IntoIterator<Item = Format>) {
        if self.dmabuf.is_some() {
            return;
        }
        let mut state = DmabufState::new();
        let global = state.create_global::<State>(&self.display_handle, formats);
        self.dmabuf = Some((state, global));
    }

    fn configure_output(&mut self, name: String, config: OutputConfig) {
        self.output_configs.entry(name.clone()).or_default().merge(config);
        self.apply_output_config(&name);
    }

    /// Bring an output in line with what the config asked for it.
    pub fn apply_output_config(&mut self, name: &str) {
        let Some(config) = self.output_configs.get(name).cloned() else { return };
        let Some(output) = self.outputs.iter().find(|o| o.name() == name).cloned() else { return };
        let mut changed = false;

        if let Some((w, h, hz)) = config.mode {
            match find_mode(&output.modes(), w, h, hz) {
                Some(mode) if Some(mode) != output.current_mode() => {
                    let ok = if self.udev.is_some() {
                        self.udev_set_mode(&output, mode)
                    } else {
                        warn!("{name}: the mode of a nested output follows its window size");
                        false
                    };
                    if ok {
                        output.change_current_state(Some(mode), None, None, None);
                        info!("{name}: mode {}", format_mode(&mode));
                        changed = true;
                    }
                }
                Some(_) => {}
                None => warn!("{name}: no {w}x{h} mode"),
            }
        }
        if let Some(scale) = config.scale
            && output.current_scale().fractional_scale() != scale
        {
            output.change_current_state(None, None, Some(smithay::output::Scale::Fractional(scale)), None);
            changed = true;
        }
        if let Some((x, y)) = config.position
            && output.current_location() != (x, y).into()
        {
            output.change_current_state(None, None, None, Some((x, y).into()));
            changed = true;
        }

        if changed {
            self.update_outputs();
            self.emit(Event::OutputChange);
        }
    }

    /// output.frame for the config, if it listens: right before the output
    /// is drawn, so what the handler changes shows in this frame.
    pub fn emit_frame(&mut self, output: &Output) {
        if !self.lua.as_ref().is_some_and(|rt| rt.wants("output.frame")) {
            return;
        }
        let name = output.name();
        let now = Instant::now();
        let dt = self
            .last_frame
            .insert(name.clone(), now)
            .map_or(0.0, |t| now.duration_since(t).as_secs_f64());
        self.emit(Event::OutputFrame { output: name, dt });
    }

    /// Work before every frame: animations, their callbacks, the commands
    /// they queued, closed windows nobody shows, and the pointer focus.
    pub fn pre_frame(&mut self) {
        let tick = self.shared.borrow_mut().scene.tick(Instant::now());
        if let Some(rt) = &self.lua {
            rt.animations_done(&tick);
        }
        self.process_commands();
        {
            let mut sh = self.shared.borrow_mut();
            sh.collect_closed();
            sh.notes.expire(Instant::now());
        }
        self.x11_frame();
        self.refresh_pointer_focus();
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
        // bars and launchers too, so the session stays usable
        if let Event::LayerReady(id) = *ev {
            let mut sh = self.shared.borrow_mut();
            let Some(l) = sh.layers.get(&id) else { return };
            if matches!(l.layer(), WlrLayer::Top | WlrLayer::Overlay) {
                let (node, x, y) = (l.node, l.geo.loc.x as f64, l.geo.loc.y as f64);
                let keyboard = l.wants_keyboard();
                let root = sh.scene.root;
                let _ = sh.scene.add_child(root, node);
                sh.scene.set_prop_now(node, crate::scene::Prop::X, x);
                sh.scene.set_prop_now(node, crate::scene::Prop::Y, y);
                if keyboard {
                    sh.commands.push(Command::Focus(Some(id)));
                }
            }
            return;
        }
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
                    Command::Mouse(s) => {
                        self.mouse.merge(s);
                        self.apply_pointer_settings();
                    }
                    Command::Touchpad(s) => {
                        self.touchpad.merge(s);
                        self.apply_pointer_settings();
                    }
                    Command::Warp(x, y) => self.warp_pointer(x, y),
                    Command::Click { id, x, y, button } => self.inject_click(id, x, y, button),
                    Command::Press { id, x, y, button } => self.inject_press(id, x, y, button),
                    Command::Drag { id, x, y } => self.inject_drag(id, x, y),
                    Command::Release { id, button } => self.inject_release(id, button),
                    Command::Keys(id, strokes) => self.inject_keys(id, &strokes),
                    Command::Reload => match self.reload() {
                        Ok(src) => info!("config reloaded ({src})"),
                        Err(e) => error!("config reload failed, keeping the old one:\n{e}"),
                    },
                    Command::Autoreload(on) => {
                        self.set_autoreload(Some(on));
                    }
                    Command::ConfigureOutput(name, config) => self.configure_output(name, config),
                    Command::Quit => self.loop_signal.stop(),
                }
            }
        }
    }

    /// The keyboard goes to a client or a layer surface, as the config says.
    pub fn set_focus(&mut self, id: Option<u64>) {
        let target = id.and_then(|id| {
            let sh = self.shared.borrow();
            match sh.clients.get(&id) {
                Some(e) => e.focus_target(),
                None => sh
                    .layers
                    .get(&id)
                    .filter(|l| l.wants_keyboard())
                    .map(|l| FocusTarget::Surface(l.surface.wl_surface().clone())),
            }
        });
        self.keyboard_focus = if target.is_some() { id } else { None };
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
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
            self.refresh_key_table();
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
        let chosen = self.chosen.as_ref().map(|c| c.path.clone());
        match self.load_config(config::candidates(chosen.as_deref())) {
            Ok(src) => info!("config loaded ({src})"),
            Err(e) => {
                error!("no config could be loaded, running in emergency mode:\n{e}");
                let text = format!("no config could be loaded; windows just stack up, Ctrl+Alt+Backspace quits\n{e}");
                self.notify(Level::Error, &text);
            }
        }
    }

    /// Reload the highest-priority config. If it fails, the running config
    /// stays as it was.
    pub fn reload(&mut self) -> Result<String, String> {
        let chosen = self.chosen.as_ref().map(|c| c.path.clone());
        let first = config::candidates(chosen.as_deref()).into_iter().take(1).collect();
        self.load_config(first)
    }

    /// Switch to another config; on failure the running one stays.
    pub fn set_config(&mut self, path: PathBuf, temp: bool) -> Result<String, String> {
        let src = self.load_config(vec![ConfigSource::File(path.clone())])?;
        self.chosen = Some(Chosen { path, temp });
        Ok(src)
    }

    /// `mywm get config`: what runs and why.
    pub fn describe_config(&self) -> String {
        let loaded = match &self.loaded {
            Some(p) => p.display().to_string(),
            None => "built-in default config".into(),
        };
        match &self.chosen {
            Some(c) if self.loaded.as_ref() != Some(&c.path) => {
                format!("{loaded} (fallback: {} failed to load)", c.path.display())
            }
            Some(c) if c.temp => {
                let saved = config::saved().map_or("none".into(), |p| p.display().to_string());
                format!("{loaded} (temp, saved: {saved})")
            }
            Some(_) => format!("{loaded} (saved)"),
            None => format!("{loaded} (default)"),
        }
    }

    /// None toggles. Returns the new value.
    pub fn set_autoreload(&mut self, on: Option<bool>) -> bool {
        let on = on.unwrap_or(!self.autoreload);
        if on && !self.autoreload {
            self.autoreload_stamp = config::stamp(&self.config_dir());
        }
        self.autoreload = on;
        on
    }

    fn config_dir(&self) -> PathBuf {
        match &self.loaded {
            Some(p) => p.parent().map(PathBuf::from).unwrap_or_default(),
            None => config::user_config_dir(),
        }
    }

    /// Called by a timer: reload when a Lua file of the config changed.
    pub fn check_autoreload(&mut self) {
        if !self.autoreload {
            return;
        }
        let stamp = config::stamp(&self.config_dir());
        if stamp == self.autoreload_stamp {
            return;
        }
        self.autoreload_stamp = stamp;
        match self.reload() {
            Ok(src) => info!("config changed, reloaded ({src})"),
            Err(e) => error!("config changed but failed to load, keeping the old one:\n{e}"),
        }
    }

    fn load_config(&mut self, sources: Vec<ConfigSource>) -> Result<String, String> {
        let generation = self.lua_gen + 1;
        let mut errors = Vec::new();
        for src in sources {
            match LuaRuntime::new(self.shared.clone(), generation, &src, self.nested) {
                Ok(rt) => {
                    self.loaded = match &src {
                        ConfigSource::File(p) => Some(p.clone()),
                        ConfigSource::Embedded => None,
                    };
                    self.shared.borrow_mut().notes.clear_config(generation);
                    if !errors.is_empty() {
                        let text = format!("{}\nrunning {} instead", errors.join("\n"), src.describe());
                        self.notify(Level::Error, &text);
                    }
                    self.install_runtime(rt, generation);
                    return Ok(src.describe());
                }
                Err(e) => {
                    self.discard_generation(generation);
                    self.shared.borrow_mut().notes.drop_owner(generation);
                    errors.push(format!("{}: {e}", src.describe()));
                }
            }
        }
        let errors = errors.join("\n");
        if self.lua.is_some() {
            self.notify(Level::Error, &format!("{errors}\nthe running config stays"));
        }
        Err(errors)
    }

    /// A message from the core on screen.
    pub fn notify(&mut self, level: Level, text: &str) {
        self.shared.borrow_mut().notes.push(level, text, level.default_timeout(), None);
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
        self.reload_cursor();
        // a setting the new config leaves out goes back to the device default
        self.mouse = PointerSettings::default();
        self.touchpad = PointerSettings::default();

        // the new config sees the monitors first (it may set their modes)...
        self.output_configs.clear();
        let names: Vec<String> = self.outputs.iter().map(|o| o.name()).collect();
        for name in names {
            self.emit(Event::OutputAdded(name));
        }

        // ...then adopts the windows that are already open
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

        // and the bars and wallpapers
        let mut layers: Vec<(LayerId, NodeId, Stage)> =
            self.shared.borrow().layers.iter().map(|(id, l)| (*id, l.node, l.stage)).collect();
        layers.sort_unstable_by_key(|(id, _, _)| *id);
        for (id, node, stage) in layers {
            self.emit(Event::LayerNew(id, node));
            if stage == Stage::Ready {
                self.emit(Event::LayerReady(id));
            }
        }
        self.process_commands();
        self.apply_pointer_settings();
    }

    /// libinput settings go to the devices; nested, the host owns them.
    fn apply_pointer_settings(&mut self) {
        if let Some(udev) = self.udev.as_mut() {
            udev.configure_devices(&self.mouse, &self.touchpad);
        }
    }

    /// An environment variable as the running config sets it (wm.env).
    pub fn env_var(&self, name: &str) -> Option<String> {
        match &self.lua {
            Some(rt) => rt.env_var(name),
            None => std::env::var(name).ok(),
        }
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
