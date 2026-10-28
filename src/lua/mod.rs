//! The Lua side: one Lua state per loaded config, the raw `core` functions
//! the prelude builds `wm` from, and scene nodes as userdata.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    time::{Duration, Instant},
};

use mlua::{
    AnyUserData, Function, HookTriggers, IntoLua, Lua, LuaOptions, MetaMethod, MultiValue, StdLib, Table,
    UserData, UserDataMethods, Value, VmState,
};
use tracing::{error, info};

use crate::{
    config::ConfigSource,
    notify::Level,
    scene::{self, Easing, NodeId, NodeKind, Prop, TickResult},
    state::{
        Accel, Command, Event, KeyboardSettings, OutputConfig, OutputMode, ParentKey, PointerSettings, Request, Shared,
        Stage, WindowState, find_mode, format_mode,
    },
};

const PRELUDE: &str = include_str!("prelude.lua");
const DEFAULT_CONFIG: &str = include_str!("../../config/init.lua");

/// The standard library, for `require("mywm.<name>")`.
const STDLIB: &[(&str, &str)] = &[
    ("mywm.std", include_str!("std/init.lua")),
    ("mywm.desktop", include_str!("std/desktop.lua")),
    ("mywm.layouts", include_str!("std/layouts.lua")),
    ("mywm.binds", include_str!("std/binds.lua")),
];

/// How long one call into Lua may run before the watchdog stops it.
const CALL_LIMIT: Duration = Duration::from_secs(1);
const LOAD_LIMIT: Duration = Duration::from_secs(5);

/// Lua 5.4 has integers and floats; both are numbers here.
fn number_of(v: &Value) -> Option<f64> {
    match *v {
        Value::Integer(i) => Some(i as f64),
        Value::Number(n) => Some(n),
        _ => None,
    }
}

fn rt_err(msg: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(msg.into())
}

/// Error text without mlua's wrapping, e.g. "init.lua:12: attempt to ...".
fn error_text(e: &mlua::Error) -> String {
    match e {
        mlua::Error::RuntimeError(s) | mlua::Error::SyntaxError { message: s, .. } => s.clone(),
        mlua::Error::CallbackError { traceback, cause } => format!("{}\n{traceback}", error_text(cause)),
        other => other.to_string(),
    }
}

/// State shared by every closure and userdata of one Lua runtime.
#[derive(Clone)]
struct Ctx {
    shared: Rc<RefCell<Shared>>,
    generation: u32,
    /// `mywm check`: run the config without side effects
    dry_run: bool,
    /// wm.env: variables for the apps the config starts (None = unset)
    env: Rc<RefCell<HashMap<String, Option<String>>>>,
    /// one userdata per node, so a node keeps its identity in Lua
    nodes: Rc<RefCell<HashMap<NodeId, AnyUserData>>>,
    /// animation group -> its `done` callback
    done: Rc<RefCell<HashMap<u64, Function>>>,
    /// events that cost a Lua call even without handlers are only sent
    /// while the config has some (output.frame)
    subscribed: Rc<RefCell<HashSet<String>>>,
}

impl Ctx {
    fn node_ud(&self, lua: &Lua, id: NodeId) -> mlua::Result<AnyUserData> {
        if let Some(ud) = self.nodes.borrow().get(&id) {
            return Ok(ud.clone());
        }
        let ud = lua.create_userdata(LuaNode { id, ctx: self.clone() })?;
        self.nodes.borrow_mut().insert(id, ud.clone());
        Ok(ud)
    }

    fn prune_nodes(&self) {
        let sh = self.shared.borrow();
        self.nodes.borrow_mut().retain(|id, _| sh.scene.exists(*id));
    }
}

struct LuaNode {
    id: NodeId,
    ctx: Ctx,
}

const NODE_PROPS: &str = "x, y, w, h, opacity, visible, input, color, kind, parent";

impl LuaNode {
    fn check(&self) -> mlua::Result<()> {
        if self.ctx.shared.borrow().scene.exists(self.id) {
            Ok(())
        } else {
            Err(rt_err("this node was destroyed"))
        }
    }

    fn destroy_with(&self, f: impl FnOnce(&mut scene::Scene, &dyn Fn(u64) -> bool) -> Vec<u64>) {
        {
            let mut sh = self.ctx.shared.borrow_mut();
            let Shared { scene, clients, .. } = &mut *sh;
            let is_live = |c: u64| clients.get(&c).is_some_and(|e| e.stage != Stage::Closed);
            let freed = f(scene, &is_live);
            for c in freed {
                clients.remove(&c);
            }
        }
        self.ctx.prune_nodes();
    }
}

impl UserData for LuaNode {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("add", |_, this, child: AnyUserData| {
            let child_id = child
                .borrow::<LuaNode>()
                .map_err(|_| rt_err("node:add: expected a node"))?
                .id;
            this.ctx
                .shared
                .borrow_mut()
                .scene
                .add_child(this.id, child_id)
                .map_err(|e| rt_err(format!("node:add: {e}")))
        });

        m.add_method("remove", |_, this, ()| {
            this.check()?;
            this.ctx.shared.borrow_mut().scene.detach(this.id);
            Ok(())
        });

        m.add_method("destroy", |_, this, ()| {
            if this.id == this.ctx.shared.borrow().scene.root {
                return Err(rt_err("node:destroy: the root node cannot be destroyed"));
            }
            this.destroy_with(|scene, live| scene.destroy(this.id, live));
            Ok(())
        });

        m.add_method("clear", |_, this, ()| {
            this.check()?;
            this.destroy_with(|scene, live| scene.clear(this.id, live));
            Ok(())
        });

        m.add_method("animate", |_, this, t: Table| {
            this.check()?;
            let mut targets = Vec::new();
            let mut duration = 0.25;
            let mut easing = Easing::OutCubic;
            let mut done = None;
            for pair in t.pairs::<String, Value>() {
                let (k, v) = pair?;
                match k.as_str() {
                    "duration" => {
                        duration = number_of(&v).ok_or_else(|| rt_err("node:animate: duration must be a number"))?
                    }
                    "easing" => {
                        let name = v.as_string().and_then(|s| s.to_str().ok().map(|s| s.to_string()));
                        easing = name
                            .as_deref()
                            .and_then(Easing::parse)
                            .ok_or_else(|| rt_err("node:animate: easing must be one of wm.easing"))?;
                    }
                    "done" => match v {
                        Value::Function(f) => done = Some(f),
                        _ => return Err(rt_err("node:animate: done must be a function")),
                    },
                    name => {
                        let prop = Prop::parse(name).ok_or_else(|| {
                            rt_err(format!(
                                "node:animate: cannot animate '{name}' (animatable: x, y, w, h, opacity; \
                                 options: duration, easing, done)"
                            ))
                        })?;
                        let to = number_of(&v).ok_or_else(|| rt_err(format!("node:animate: {name} must be a number")))?;
                        targets.push((prop, to));
                    }
                }
            }
            let group = this.ctx.shared.borrow_mut().scene.animate(this.id, &targets, duration, easing);
            if let Some(f) = done {
                this.ctx.done.borrow_mut().insert(group, f);
            }
            Ok(())
        });

        m.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            this.check()?;
            let sh = this.ctx.shared.borrow();
            let n = &sh.scene.nodes[this.id];
            let v = match key.as_str() {
                "x" => n.x.into_lua(lua)?,
                "y" => n.y.into_lua(lua)?,
                "w" => n.w.into_lua(lua)?,
                "h" => n.h.into_lua(lua)?,
                "opacity" => n.opacity.into_lua(lua)?,
                "visible" => Value::Boolean(n.visible),
                "input" => Value::Boolean(n.input),
                "kind" => n.kind.name().into_lua(lua)?,
                "color" => match &n.kind {
                    NodeKind::Rect { color, .. } => scene::format_color(*color).into_lua(lua)?,
                    _ => return Err(rt_err("only rect nodes have a color")),
                },
                "parent" => match n.parent {
                    Some(p) => {
                        drop(sh);
                        Value::UserData(this.ctx.node_ud(lua, p)?)
                    }
                    None => Value::Nil,
                },
                other => return Err(rt_err(format!("node has no property '{other}' (properties: {NODE_PROPS})"))),
            };
            Ok(v)
        });

        m.add_meta_method(MetaMethod::NewIndex, |_, this, (key, value): (String, Value)| {
            this.check()?;
            let mut sh = this.ctx.shared.borrow_mut();
            let scene = &mut sh.scene;
            let number = |v: &Value| {
                number_of(&v)
                    .ok_or_else(|| rt_err(format!("node.{key} must be a number")))
            };
            let boolean = |v: &Value| {
                v.as_boolean()
                    .ok_or_else(|| rt_err(format!("node.{key} must be a boolean")))
            };
            match key.as_str() {
                "x" | "y" | "w" | "h" | "opacity" => {
                    if matches!(key.as_str(), "w" | "h")
                        && matches!(scene.nodes[this.id].kind, NodeKind::Client(_) | NodeKind::Layer(_))
                    {
                        return Err(rt_err("the size of a client or layer node is set by its app"));
                    }
                    let prop = Prop::parse(&key).unwrap();
                    scene.set_prop_now(this.id, prop, number(&value)?);
                }
                "visible" => scene.nodes[this.id].visible = boolean(&value)?,
                "input" => scene.nodes[this.id].input = boolean(&value)?,
                "color" => {
                    let s = value
                        .as_string()
                        .and_then(|s| s.to_str().ok().map(|s| s.to_string()))
                        .ok_or_else(|| rt_err("node.color must be a string like \"#rrggbb\" or \"#rrggbbaa\""))?;
                    let rgba = scene::parse_color(&s)
                        .ok_or_else(|| rt_err(format!("invalid color '{s}' (expected #rrggbb or #rrggbbaa)")))?;
                    if !scene.set_color(this.id, rgba) {
                        return Err(rt_err("only rect nodes have a color"));
                    }
                }
                "kind" | "parent" => {
                    return Err(rt_err(format!("node.{key} is read-only (use node:add to change the parent)")));
                }
                other => return Err(rt_err(format!("node has no property '{other}' (properties: {NODE_PROPS})"))),
            }
            Ok(())
        });

        m.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| {
            Ok(other.borrow::<LuaNode>().map(|o| o.id == this.id).unwrap_or(false))
        });

        m.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            let sh = this.ctx.shared.borrow();
            Ok(match sh.scene.nodes.get(this.id) {
                Some(n) => format!("node ({})", n.kind.name()),
                None => "node (destroyed)".to_string(),
            })
        });
    }
}

pub struct LuaRuntime {
    lua: Lua,
    dispatch: Function,
    deadline: Rc<Cell<Option<Instant>>>,
    ctx: Ctx,
}

impl LuaRuntime {
    /// Create a Lua state, run the prelude and then the config.
    pub fn new(shared: Rc<RefCell<Shared>>, generation: u32, source: &ConfigSource, nested: bool) -> Result<Self, String> {
        Self::create(shared, generation, source, nested, false)
    }

    /// Run a config against a throwaway state: catches syntax errors, typos
    /// in constants and wrong API use at load time, without spawning anything.
    pub fn check(source: &ConfigSource) -> Result<(), String> {
        let shared = Rc::new(RefCell::new(Shared::new(Instant::now())));
        Self::create(shared, 1, source, true, true).map(|_| ())
    }

    fn create(
        shared: Rc<RefCell<Shared>>,
        generation: u32,
        source: &ConfigSource,
        nested: bool,
        dry_run: bool,
    ) -> Result<Self, String> {
        let lua = unsafe { Lua::unsafe_new_with(StdLib::ALL_SAFE | StdLib::DEBUG, LuaOptions::default()) };
        let ctx = Ctx {
            shared,
            generation,
            dry_run,
            env: Rc::default(),
            subscribed: Rc::default(),
            nodes: Rc::default(),
            done: Rc::default(),
        };
        let deadline = Rc::new(Cell::new(None::<Instant>));

        let watchdog = deadline.clone();
        lua.set_hook(HookTriggers::new().every_nth_instruction(10_000), move |_, _| {
            match watchdog.get() {
                Some(d) if Instant::now() > d => Err(rt_err(
                    "Lua code ran for too long and was stopped (an endless loop?)",
                )),
                _ => Ok(VmState::Continue),
            }
        })
        .map_err(|e| e.to_string())?;

        let rt = (|| -> mlua::Result<LuaRuntime> {
            let dir = source.dir();
            let package: Table = lua.globals().get("package")?;
            let path: String = package.get("path")?;
            let d = dir.display();
            package.set("path", format!("{d}/?.lua;{d}/?/init.lua;{path}"))?;
            let preload: Table = package.get("preload")?;
            for &(name, code) in STDLIB {
                let chunk = format!("@{}.lua", name.replace('.', "/"));
                let loader = lua.load(code).set_name(chunk).into_function()?;
                preload.set(name, loader)?;
            }

            let core = install_core(&lua, &ctx)?;
            let info = lua.create_table()?;
            info.set("config_dir", dir.display().to_string())?;
            info.set("nested", nested)?;

            let dispatch: Function = lua.load(PRELUDE).set_name("=prelude").call((core, info))?;
            Ok(LuaRuntime {
                lua: lua.clone(),
                dispatch,
                deadline: deadline.clone(),
                ctx: ctx.clone(),
            })
        })()
        .map_err(|e| error_text(&e))?;

        let (code, name) = match source {
            ConfigSource::File(p) => (
                std::fs::read_to_string(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?,
                format!("@{}", p.file_name().unwrap_or_default().to_string_lossy()),
            ),
            ConfigSource::Embedded => (DEFAULT_CONFIG.to_string(), "@default/init.lua".to_string()),
        };
        rt.deadline.set(Some(Instant::now() + LOAD_LIMIT));
        let result = rt.lua.load(code).set_name(name).exec();
        rt.deadline.set(None);
        result.map_err(|e| error_text(&e))?;
        Ok(rt)
    }

    fn guarded<T>(&self, f: impl FnOnce() -> mlua::Result<T>) -> mlua::Result<T> {
        self.deadline.set(Some(Instant::now() + CALL_LIMIT));
        let r = f();
        self.deadline.set(None);
        r
    }

    /// Run the handlers of an event. Returns true if one of them consumed it.
    pub fn dispatch(&self, ev: &Event) -> bool {
        let lua = &self.lua;
        let args = (|| -> mlua::Result<MultiValue> {
            let mut args = MultiValue::new();
            let mut push = |v: Value| args.push_back(v);
            match *ev {
                Event::ClientNew(id, node) => {
                    push("client.new".into_lua(lua)?);
                    push(Value::Integer(id as i64));
                    push(Value::UserData(self.ctx.node_ud(lua, node)?));
                }
                Event::ClientReady(id) => {
                    push("client.ready".into_lua(lua)?);
                    push(Value::Integer(id as i64));
                }
                Event::ClientClosed(id) => {
                    push("client.closed".into_lua(lua)?);
                    push(Value::Integer(id as i64));
                }
                Event::ClientResize(id) => {
                    push("client.resize".into_lua(lua)?);
                    push(Value::Integer(id as i64));
                }
                Event::LayerNew(id, node) => {
                    push("layer.new".into_lua(lua)?);
                    push(Value::Integer(id as i64));
                    push(Value::UserData(self.ctx.node_ud(lua, node)?));
                }
                Event::LayerReady(id) => {
                    push("layer.ready".into_lua(lua)?);
                    push(Value::Integer(id as i64));
                }
                Event::LayerChange(id) => {
                    push("layer.change".into_lua(lua)?);
                    push(Value::Integer(id as i64));
                }
                Event::LayerClosed(id, gone) => {
                    push("layer.closed".into_lua(lua)?);
                    push(Value::Integer(id as i64));
                    push(Value::Boolean(gone));
                }
                Event::ClientTitle(id) => {
                    push("client.title".into_lua(lua)?);
                    push(Value::Integer(id as i64));
                }
                Event::ClientHover(id, entered) => {
                    push(if entered { "client.enter" } else { "client.leave" }.into_lua(lua)?);
                    push(Value::Integer(id as i64));
                }
                Event::ClientRequest(id, ref request) => {
                    let (name, a, b) = match *request {
                        Request::Fullscreen(on) => ("client.request_fullscreen", Value::Boolean(on), Value::Nil),
                        Request::Maximize(on) => ("client.request_maximize", Value::Boolean(on), Value::Nil),
                        Request::Minimize => ("client.request_minimize", Value::Nil, Value::Nil),
                        Request::Move(button) => ("client.request_move", Value::Integer(button as i64), Value::Nil),
                        Request::Resize(button, edges) => (
                            "client.request_resize",
                            Value::Integer(button as i64),
                            Value::Integer(edges as i64),
                        ),
                    };
                    push(name.into_lua(lua)?);
                    push(Value::Integer(id as i64));
                    push(a);
                    push(b);
                }
                Event::OutputAdded(ref name) => {
                    push("output.added".into_lua(lua)?);
                    push(name.as_str().into_lua(lua)?);
                }
                Event::OutputRemoved(ref name) => {
                    push("output.removed".into_lua(lua)?);
                    push(name.as_str().into_lua(lua)?);
                }
                Event::OutputChange => push("output.change".into_lua(lua)?),
                Event::OutputFrame { ref output, dt } => {
                    push("output.frame".into_lua(lua)?);
                    push(output.as_str().into_lua(lua)?);
                    push(Value::Number(dt));
                }
                Event::Key { key, mods, pressed } => {
                    push("input.key".into_lua(lua)?);
                    push(Value::Integer(key as i64));
                    push(Value::Integer(mods as i64));
                    push(Value::Boolean(pressed));
                }
                Event::Button { x, y, button, pressed, mods } => {
                    push("input.button".into_lua(lua)?);
                    push(Value::Number(x));
                    push(Value::Number(y));
                    push(Value::Integer(button as i64));
                    push(Value::Boolean(pressed));
                    push(Value::Integer(mods as i64));
                }
                Event::Motion { x, y, dx, dy, mods } => {
                    push("input.motion".into_lua(lua)?);
                    push(Value::Number(x));
                    push(Value::Number(y));
                    push(Value::Number(dx));
                    push(Value::Number(dy));
                    push(Value::Integer(mods as i64));
                }
                Event::Scroll { x, y, dx, dy, wheel, mods } => {
                    push("input.scroll".into_lua(lua)?);
                    push(Value::Number(x));
                    push(Value::Number(y));
                    push(Value::Number(dx));
                    push(Value::Number(dy));
                    push(Value::Integer(mods as i64));
                    push(Value::Boolean(wheel));
                }
            }
            Ok(args)
        })();
        let result = args.and_then(|args| self.guarded(|| self.dispatch.call::<bool>(args)));
        match result {
            Ok(consumed) => consumed,
            Err(e) => {
                let text = error_text(&e);
                error!("event dispatch failed: {text}");
                self.ctx.shared.borrow_mut().notes.push(Level::Error, &text, None, None);
                false
            }
        }
    }

    /// Call the `done` callbacks of finished animations.
    pub fn animations_done(&self, tick: &TickResult) {
        for g in &tick.cancelled {
            self.ctx.done.borrow_mut().remove(g);
        }
        for g in &tick.finished {
            let f = self.ctx.done.borrow_mut().remove(g);
            if let Some(f) = f
                && let Err(e) = self.guarded(|| f.call::<()>(()))
            {
                let text = format!("animation done callback: {}", error_text(&e));
                error!("{text}");
                self.ctx.shared.borrow_mut().notes.push(Level::Error, &text, None, None);
            }
        }
    }

    /// Run a chunk of Lua (from `mywm eval`) and return its results as text.
    pub fn eval(&self, code: &str) -> Result<String, String> {
        let values = self
            .guarded(|| self.lua.load(code).set_name("=eval").eval::<MultiValue>())
            .map_err(|e| error_text(&e))?;
        let tostring: Function = self.lua.globals().get("tostring").map_err(|e| e.to_string())?;
        let parts = values
            .into_iter()
            .map(|v| tostring.call::<String>(v))
            .collect::<mlua::Result<Vec<_>>>()
            .map_err(|e| error_text(&e))?;
        Ok(parts.join("\t"))
    }
}

impl LuaRuntime {
    /// Whether the config has handlers for an event (see Ctx::subscribed).
    pub fn wants(&self, event: &str) -> bool {
        self.ctx.subscribed.borrow().contains(event)
    }

    /// A variable as the config's apps see it: wm.env first, then MyWM's own.
    pub fn env_var(&self, name: &str) -> Option<String> {
        match self.ctx.env.borrow().get(name) {
            Some(v) => v.clone(),
            None => std::env::var(name).ok(),
        }
    }
}

impl Drop for LuaRuntime {
    fn drop(&mut self) {
        // the cached userdata hold the context; break the cycle
        self.ctx.nodes.borrow_mut().clear();
        self.ctx.done.borrow_mut().clear();
    }
}

fn opt_string(t: &Table, key: &str) -> mlua::Result<Option<String>> {
    t.get::<Option<String>>(key)
}

/// The raw functions the prelude builds the `wm` table from.
fn install_core(lua: &Lua, ctx: &Ctx) -> mlua::Result<Table> {
    let core = lua.create_table()?;

    let c = ctx.clone();
    core.set("root", lua.create_function(move |lua, ()| {
        let root = c.shared.borrow().scene.root;
        c.node_ud(lua, root)
    })?)?;

    let c = ctx.clone();
    core.set("node_new", lua.create_function(move |lua, kind: String| {
        let kind = match kind.as_str() {
            "group" => NodeKind::Group,
            "rect" => NodeKind::Rect {
                color: [1.0, 1.0, 1.0, 1.0],
                buffer: Default::default(),
            },
            other => return Err(rt_err(format!("unknown node kind '{other}'"))),
        };
        let id = c.shared.borrow_mut().scene.create(kind, c.generation);
        c.node_ud(lua, id)
    })?)?;

    core.set("easings", lua.create_function(|_, ()| Ok(Easing::NAMES.to_vec()))?)?;

    let c = ctx.clone();
    core.set("client_ids", lua.create_function(move |_, ()| {
        let sh = c.shared.borrow();
        let mut ids: Vec<u64> = sh
            .clients
            .iter()
            .filter(|(_, e)| matches!(e.stage, Stage::New | Stage::Ready))
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        Ok(ids)
    })?)?;

    let c = ctx.clone();
    core.set("client_get", lua.create_function(move |lua, (id, field): (u64, String)| {
        let sh = c.shared.borrow();
        let Some(e) = sh.clients.get(&id) else {
            return Ok(Value::Nil);
        };
        Ok(match field.as_str() {
            "title" => e.title.clone().into_lua(lua)?,
            "app_id" => e.app_id.clone().into_lua(lua)?,
            "width" => e.size.w.into_lua(lua)?,
            "height" => e.size.h.into_lua(lua)?,
            "closed" => Value::Boolean(e.stage == Stage::Closed),
            "x11" => Value::Boolean(e.window.x11_surface().is_some()),
            "pid" => match e.window.x11_surface() {
                Some(x) => x.pid().into_lua(lua)?,
                None => e.pid.into_lua(lua)?,
            },
            "parent" => {
                let parent = match e.parent_key() {
                    Some(ParentKey::Surface(s)) => sh.client_by_surface(&s),
                    Some(ParentKey::X11(w)) => sh
                        .clients
                        .iter()
                        .find(|(_, p)| p.window.x11_surface().is_some_and(|x| x.window_id() == w))
                        .map(|(id, _)| *id),
                    None => None,
                };
                parent.into_lua(lua)?
            }
            "min_width" | "min_height" | "max_width" | "max_height" => {
                let (min, max) = e.size_hints();
                let v = match field.as_str() {
                    "min_width" => min.w,
                    "min_height" => min.h,
                    "max_width" => max.w,
                    _ => max.h,
                };
                Value::Integer(v as i64)
            }
            "fullscreen" => Value::Boolean(e.fullscreen),
            "maximized" => Value::Boolean(e.maximized),
            "tiled" => Value::Boolean(e.tiled),
            "window_type" => match e.window.x11_surface().and_then(|x| x.window_type()) {
                Some(t) => format!("{t:?}").to_lowercase().into_lua(lua)?,
                None => Value::Nil,
            },
            _ => Value::Nil,
        })
    })?)?;

    let c = ctx.clone();
    core.set("client_configure", lua.create_function(move |_, (id, w, h): (u64, Option<f64>, Option<f64>)| {
        if let Some(e) = c.shared.borrow_mut().live_client(id) {
            let size = |v: Option<f64>| v.unwrap_or(0.0).round().max(0.0) as i32;
            e.configure(size(w), size(h));
        }
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("client_close", lua.create_function(move |_, id: u64| {
        if let Some(e) = c.shared.borrow_mut().live_client(id) {
            e.close();
        }
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("client_click", lua.create_function(move |_, (id, x, y, button): (u64, f64, f64, u32)| {
        let mut sh = c.shared.borrow_mut();
        if sh.client_surface_at(id, (x, y).into()).is_none() {
            return Ok(false);
        }
        sh.commands.push(Command::Click { id, x, y, button });
        Ok(true)
    })?)?;

    let c = ctx.clone();
    core.set("client_type", lua.create_function(move |_, (id, text): (u64, String)| {
        let mut sh = c.shared.borrow_mut();
        let strokes = sh.keys.text(&text).map_err(|ch| {
            rt_err(format!("client:type: no key types {ch:?} in the current keyboard layouts"))
        })?;
        if sh.live_client(id).is_some() && !strokes.is_empty() {
            sh.commands.push(Command::Keys(id, strokes));
        }
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("client_key", lua.create_function(move |_, (id, sym, mods): (u64, u32, Option<u32>)| {
        let mut sh = c.shared.borrow_mut();
        let stroke = sh
            .keys
            .key(sym, mods.unwrap_or(0))
            .ok_or_else(|| rt_err(format!("client:key: no key produces keysym {sym:#x} in the current keyboard layouts")))?;
        if sh.live_client(id).is_some() {
            sh.commands.push(Command::Keys(id, vec![stroke]));
        }
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("layer_get", lua.create_function(move |lua, (id, field): (u64, String)| {
        let sh = c.shared.borrow();
        let Some(l) = sh.layers.get(&id) else {
            return Ok(if field == "closed" { Value::Boolean(true) } else { Value::Nil });
        };
        let state = l.surface.cached_state();
        Ok(match field.as_str() {
            "namespace" => l.surface.namespace().into_lua(lua)?,
            "layer" => format!("{:?}", l.layer()).to_lowercase().into_lua(lua)?,
            "output" => l.output.clone().into_lua(lua)?,
            "x" => l.geo.loc.x.into_lua(lua)?,
            "y" => l.geo.loc.y.into_lua(lua)?,
            "width" => l.geo.size.w.into_lua(lua)?,
            "height" => l.geo.size.h.into_lua(lua)?,
            "keyboard" => match state.keyboard_interactivity {
                smithay::wayland::shell::wlr_layer::KeyboardInteractivity::None => "none",
                smithay::wayland::shell::wlr_layer::KeyboardInteractivity::Exclusive => "exclusive",
                smithay::wayland::shell::wlr_layer::KeyboardInteractivity::OnDemand => "on_demand",
            }
            .into_lua(lua)?,
            "exclusive_zone" => match state.exclusive_zone {
                smithay::wayland::shell::wlr_layer::ExclusiveZone::Exclusive(n) => Value::Integer(n as i64),
                smithay::wayland::shell::wlr_layer::ExclusiveZone::Neutral => Value::Integer(0),
                smithay::wayland::shell::wlr_layer::ExclusiveZone::DontCare => Value::Integer(-1),
            },
            "closed" => Value::Boolean(l.stage != Stage::Ready),
            _ => Value::Nil,
        })
    })?)?;

    let c = ctx.clone();
    core.set("layer_at", lua.create_function(move |_, (x, y): (f64, f64)| {
        Ok(c.shared.borrow().layer_at((x, y).into()))
    })?)?;

    let c = ctx.clone();
    core.set("layer_ids", lua.create_function(move |_, ()| {
        let mut ids: Vec<u64> = c.shared.borrow().layers.keys().copied().collect();
        ids.sort_unstable();
        Ok(ids)
    })?)?;

    let c = ctx.clone();
    core.set("client_set_state", lua.create_function(move |_, (id, state, on): (u64, String, bool)| {
        let state = match state.as_str() {
            "fullscreen" => WindowState::Fullscreen,
            "maximized" => WindowState::Maximized,
            "tiled" => WindowState::Tiled,
            _ => return Err(rt_err(format!("unknown window state '{state}'"))),
        };
        if let Some(e) = c.shared.borrow_mut().live_client(id) {
            e.set_state(state, on);
        }
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("client_press", lua.create_function(move |_, (id, x, y, button): (u64, f64, f64, u32)| {
        let mut sh = c.shared.borrow_mut();
        if sh.client_surface_at(id, (x, y).into()).is_none() {
            return Ok(false);
        }
        sh.commands.push(Command::Press { id, x, y, button });
        Ok(true)
    })?)?;

    let c = ctx.clone();
    core.set("client_drag", lua.create_function(move |_, (id, x, y): (u64, f64, f64)| {
        c.shared.borrow_mut().commands.push(Command::Drag { id, x, y });
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("client_release", lua.create_function(move |_, (id, button): (u64, u32)| {
        c.shared.borrow_mut().commands.push(Command::Release { id, button });
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("notify", lua.create_function(move |_, (text, level, timeout): (String, String, Option<f64>)| {
        let level = Level::parse(&level).ok_or_else(|| rt_err(format!("wm.notify: unknown level '{level}'")))?;
        let timeout = match timeout {
            Some(t) if t <= 0.0 => None,
            Some(t) => Some(Duration::from_secs_f64(t)),
            None => level.default_timeout(),
        };
        c.shared.borrow_mut().notes.push(level, &text, timeout, Some(c.generation));
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("notify_clear", lua.create_function(move |_, ()| {
        c.shared.borrow_mut().notes.clear();
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("client_activate", lua.create_function(move |_, (id, on): (u64, bool)| {
        if let Some(e) = c.shared.borrow_mut().live_client(id) {
            e.set_activated(on);
        }
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("client_decoration", lua.create_function(move |_, (id, mode): (u64, String)| {
        let mut sh = c.shared.borrow_mut();
        let mode = crate::state::decoration_mode(&mode);
        if let Some(e) = sh.clients.get_mut(&id) {
            e.decoration = Some(mode);
        }
        if let Some(t) = sh.live_toplevel(id) {
            t.with_pending_state(|s| s.decoration_mode = Some(mode));
            if t.is_initial_configure_sent() {
                t.send_pending_configure();
            }
        }
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("focus", lua.create_function(move |_, id: Option<u64>| {
        c.shared.borrow_mut().commands.push(Command::Focus(id));
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("client_at", lua.create_function(move |_, (x, y): (f64, f64)| {
        Ok(c.shared.borrow().client_at((x, y).into()))
    })?)?;

    let c = ctx.clone();
    core.set("outputs", lua.create_function(move |lua, ()| {
        let sh = c.shared.borrow();
        let list = lua.create_table()?;
        let mode_table = |m: &OutputMode| -> mlua::Result<Table> {
            let t = lua.create_table()?;
            t.set("width", m.size.w)?;
            t.set("height", m.size.h)?;
            t.set("refresh", m.refresh as f64 / 1000.0)?;
            Ok(t)
        };
        for o in &sh.outputs {
            let t = lua.create_table()?;
            t.set("name", o.name.clone())?;
            t.set("x", o.x)?;
            t.set("y", o.y)?;
            t.set("width", o.width)?;
            t.set("height", o.height)?;
            t.set("scale", o.scale)?;
            if let Some(m) = &o.mode {
                t.set("mode", mode_table(m)?)?;
            }
            if let Some(m) = &o.preferred {
                t.set("preferred", mode_table(m)?)?;
            }
            let usable = lua.create_table()?;
            usable.set("x", o.usable.loc.x)?;
            usable.set("y", o.usable.loc.y)?;
            usable.set("width", o.usable.size.w)?;
            usable.set("height", o.usable.size.h)?;
            t.set("usable", usable)?;
            let modes = lua.create_table()?;
            for m in &o.modes {
                modes.push(mode_table(m)?)?;
            }
            t.set("modes", modes)?;
            list.push(t)?;
        }
        Ok(list)
    })?)?;

    let c = ctx.clone();
    core.set("configure_output", lua.create_function(move |_, (name, t): (String, Table)| {
        let mut config = OutputConfig::default();
        if let Some(mode) = t.get::<Option<Table>>("mode")? {
            let w: i32 = mode.get("width")?;
            let h: i32 = mode.get("height")?;
            let hz: Option<f64> = mode.get("refresh")?;
            // a known output: check the mode now, so the error points at the config
            let sh = c.shared.borrow();
            if let Some(o) = sh.outputs.iter().find(|o| o.name == name)
                && find_mode(&o.modes, w, h, hz).is_none()
            {
                let wanted = match hz {
                    Some(hz) => format!("{w}x{h}@{hz}"),
                    None => format!("{w}x{h}"),
                };
                let available: Vec<String> = o.modes.iter().map(format_mode).collect();
                return Err(rt_err(format!(
                    "wm.configure_output: {name} has no mode {wanted}; it offers {}",
                    available.join(", ")
                )));
            }
            config.mode = Some((w, h, hz));
        }
        if let Some(pos) = t.get::<Option<Table>>("position")? {
            config.position = Some((pos.get("x")?, pos.get("y")?));
        }
        if let Some(scale) = t.get::<Option<f64>>("scale")? {
            if !(0.25..=8.0).contains(&scale) {
                return Err(rt_err(format!("wm.configure_output: scale {scale} is out of range (0.25 to 8)")));
            }
            config.scale = Some(scale);
        }
        c.shared.borrow_mut().commands.push(Command::ConfigureOutput(name, config));
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("env_get", lua.create_function(move |_, name: String| {
        Ok(match c.env.borrow().get(&name) {
            Some(v) => v.clone(),
            None => std::env::var(&name).ok(),
        })
    })?)?;

    let c = ctx.clone();
    core.set("env_set", lua.create_function(move |_, (name, value): (String, Option<String>)| {
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            return Err(rt_err(format!("wm.env: invalid variable name '{name}'")));
        }
        if value.as_deref().is_some_and(|v| v.contains('\0')) {
            return Err(rt_err(format!("wm.env.{name}: the value contains a NUL byte")));
        }
        c.env.borrow_mut().insert(name, value);
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("subscribed", lua.create_function(move |_, (event, on): (String, bool)| {
        let mut set = c.subscribed.borrow_mut();
        if on {
            set.insert(event);
        } else {
            set.remove(&event);
        }
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("spawn", lua.create_function(move |_, cmd: Value| {
        let argv = match cmd {
            Value::String(s) => vec!["/bin/sh".into(), "-c".into(), s.to_str()?.to_string()],
            Value::Table(t) => t.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>()?,
            _ => return Err(rt_err("wm.spawn: expected a command string or a table of arguments")),
        };
        if c.dry_run {
            return Ok(());
        }
        crate::state::spawn(&argv, &c.env.borrow()).map_err(|e| rt_err(format!("wm.spawn: {e}")))
    })?)?;

    let c = ctx.clone();
    core.set("autoreload", lua.create_function(move |_, on: bool| {
        c.shared.borrow_mut().commands.push(Command::Autoreload(on));
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("reload", lua.create_function(move |_, ()| {
        c.shared.borrow_mut().commands.push(Command::Reload);
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("quit", lua.create_function(move |_, ()| {
        c.shared.borrow_mut().commands.push(Command::Quit);
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("keyboard", lua.create_function(move |_, t: Table| {
        let k = KeyboardSettings {
            layout: opt_string(&t, "layout")?,
            variant: opt_string(&t, "variant")?,
            options: opt_string(&t, "options")?,
            model: opt_string(&t, "model")?,
            repeat_rate: t.get::<Option<i32>>("repeat_rate")?,
            repeat_delay: t.get::<Option<i32>>("repeat_delay")?,
        };
        c.shared.borrow_mut().commands.push(Command::Keyboard(k));
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("pointer_settings", lua.create_function(move |_, (kind, t): (String, Table)| {
        let fname = format!("wm.input.{kind}");
        let speed: Option<f64> = t.get("speed")?;
        if let Some(speed) = speed
            && !(-1.0..=1.0).contains(&speed)
        {
            return Err(rt_err(format!("{fname}: speed {speed} is out of range (-1 to 1)")));
        }
        let accel = match opt_string(&t, "accel")?.as_deref() {
            None => None,
            Some("flat") => Some(Accel::Flat),
            Some("adaptive") => Some(Accel::Adaptive),
            Some(other) => return Err(rt_err(format!("{fname}: unknown accel '{other}'"))),
        };
        let s = PointerSettings {
            speed,
            accel,
            natural_scroll: t.get("natural_scroll")?,
            left_handed: t.get("left_handed")?,
            tap: t.get("tap")?,
            dwt: t.get("disable_while_typing")?,
        };
        let cmd = if kind == "touchpad" { Command::Touchpad(s) } else { Command::Mouse(s) };
        c.shared.borrow_mut().commands.push(cmd);
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("pointer", lua.create_function(move |_, ()| Ok(c.shared.borrow().pointer))?)?;

    let c = ctx.clone();
    core.set("warp", lua.create_function(move |_, (x, y): (f64, f64)| {
        c.shared.borrow_mut().commands.push(Command::Warp(x, y));
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("time", lua.create_function(move |_, ()| Ok(c.shared.borrow().start.elapsed().as_secs_f64()))?)?;

    core.set("log", lua.create_function(|_, msg: String| {
        info!(target: "lua", "{msg}");
        Ok(())
    })?)?;

    let c = ctx.clone();
    core.set("report", lua.create_function(move |_, (event, msg): (String, String)| {
        error!("{event} handler failed: {msg}");
        let text = format!("{event} handler failed: {msg}");
        c.shared.borrow_mut().notes.push(Level::Error, &text, None, None);
        Ok(())
    })?)?;

    Ok(core)
}
