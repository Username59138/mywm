mod config;
mod handlers;
mod input;
mod ipc;
mod lua;
mod render;
mod scene;
mod snapshot;
mod state;
mod udev;
mod winit;

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand, ValueEnum};
use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use tracing::{error, info};

use crate::{
    config::ConfigSource,
    lua::LuaRuntime,
    state::{Chosen, State},
};

/// MyWM: a Wayland compositor where your Lua config is the window manager.
#[derive(Parser)]
#[command(name = "mywm", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Reload the config of the running session
    Reload,
    /// Quit the running session
    Quit,
    /// Run Lua code in the running session and print what it returns
    Eval {
        /// Lua code, e.g. 'return #wm.clients()'
        code: String,
    },
    /// Change a setting
    Set {
        #[command(subcommand)]
        setting: SetCmd,
    },
    /// Show a setting of the running session
    Get {
        #[arg(value_enum)]
        setting: Setting,
    },
    /// Check a config without applying it (default: the one MyWM would load)
    Check {
        /// Config directory (or its init.lua)
        dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SetCmd {
    /// Reload the config whenever one of its Lua files changes
    Autoreload {
        /// true or false; without a value it toggles
        #[arg(value_parser = clap::value_parser!(bool))]
        value: Option<bool>,
    },
    /// Use another config directory; the choice is saved for the next start
    Config {
        /// Config directory (or its init.lua)
        dir: PathBuf,
        /// Start MyWM with it (in a window inside the current session)
        #[arg(short, long)]
        run: bool,
        /// Only for this session; keep the saved choice
        #[arg(short, long)]
        temp: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Setting {
    Autoreload,
    Config,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => run(config::saved().map(|path| Chosen { path, temp: false })),
        Some(Cmd::Reload) => remote("reload", ""),
        Some(Cmd::Quit) => remote("quit", ""),
        Some(Cmd::Eval { code }) => remote("eval", &code),
        Some(Cmd::Set { setting: SetCmd::Autoreload { value } }) => {
            let value = value.map_or("toggle", |v| if v { "true" } else { "false" });
            remote("set-autoreload", value)
        }
        Some(Cmd::Set { setting: SetCmd::Config { dir, run, temp } }) => set_config(&dir, run, temp),
        Some(Cmd::Get { setting }) => get(setting),
        Some(Cmd::Check { dir }) => check(dir),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn remote(cmd: &str, payload: &str) -> Result<(), String> {
    let out = ipc::send(cmd, payload)?;
    if !out.is_empty() {
        println!("{out}");
    }
    Ok(())
}

fn set_config(dir: &std::path::Path, run_it: bool, temp: bool) -> Result<(), String> {
    let path = config::resolve(dir)?;
    if run_it {
        if !temp {
            config::save(&path)?;
        }
        return run(Some(Chosen { path, temp }));
    }

    match ipc::session()? {
        Some(_) => {
            let payload = format!("{}\n{}", path.display(), if temp { "1" } else { "0" });
            match ipc::send("set-config", &payload) {
                Ok(out) => {
                    if !temp {
                        config::save(&path)?;
                    }
                    println!("{out}{}", if temp { " (temp)" } else { " (saved)" });
                    Ok(())
                }
                Err(e) => Err(format!("{e}\nconfig unchanged")),
            }
        }
        None if temp => Err("--temp needs a running session or --run".into()),
        None => {
            LuaRuntime::check(&ConfigSource::File(path.clone()))
                .map_err(|e| format!("{e}\nconfig not saved"))?;
            config::save(&path)?;
            println!("config: {} (saved, used at the next start)", path.display());
            Ok(())
        }
    }
}

fn get(setting: Setting) -> Result<(), String> {
    let name = match setting {
        Setting::Autoreload => "autoreload",
        Setting::Config => "config",
    };
    if ipc::session()?.is_some() {
        return remote("get", name);
    }
    match setting {
        Setting::Config => {
            match config::saved() {
                Some(p) => println!("{} (saved; MyWM is not running)", p.display()),
                None => println!("{} (default; MyWM is not running)", config::user_config_dir().display()),
            }
            Ok(())
        }
        Setting::Autoreload => Err("MyWM is not running".into()),
    }
}

fn check(dir: Option<PathBuf>) -> Result<(), String> {
    let source = match dir {
        Some(d) => ConfigSource::File(config::resolve(&d)?),
        None => config::candidates(config::saved().as_deref())
            .into_iter()
            .next()
            .unwrap_or(ConfigSource::Embedded),
    };
    LuaRuntime::check(&source)?;
    println!("ok: {}", source.describe());
    Ok(())
}

fn run(chosen: Option<Chosen>) -> Result<(), String> {
    // inside another Wayland session: a window; otherwise the real thing
    let nested = std::env::var_os("WAYLAND_DISPLAY").is_some();
    init_logging(nested)?;
    run_compositor(chosen, nested).map_err(|e| {
        error!("{e}");
        e.to_string()
    })
}

fn init_logging(nested: bool) -> Result<(), String> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,smithay=warn"));
    if nested {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return Ok(());
    }
    // on a TTY the terminal is hidden behind the compositor
    let path = config::log_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let file = std::fs::File::create(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    eprintln!("mywm: logging to {}", path.display());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .init();
    Ok(())
}

fn run_compositor(chosen: Option<Chosen>, nested: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let mut state = State::new(&mut event_loop, display, nested);
    state.chosen = chosen;

    if nested {
        // connects to the host compositor, so it must run before WAYLAND_DISPLAY changes
        winit::init(&mut event_loop, &mut state)?;
    } else {
        udev::init(&mut event_loop, &mut state)?;
    }

    let socket = ipc::socket_path(&state.socket_name);
    ipc::listen(&state.loop_handle, &socket)?;

    // Safety: set before anything is spawned; no other thread reads the environment
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
        std::env::set_var("MYWM_SOCKET", &socket);
        // there is no XWayland yet; X11 apps would open on the host
        std::env::remove_var("DISPLAY");
        if !nested {
            std::env::set_var("XDG_SESSION_TYPE", "wayland");
            std::env::set_var("XDG_CURRENT_DESKTOP", "mywm");
        }
    }
    info!("listening on WAYLAND_DISPLAY={}", state.socket_name.to_string_lossy());

    state.load_initial_config();

    event_loop.run(None, &mut state, |_| {})?;

    drop(state);
    let _ = std::fs::remove_file(&socket);
    Ok(())
}
