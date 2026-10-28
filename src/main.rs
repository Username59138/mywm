mod handlers;
mod input;
mod ipc;
mod lua;
mod scene;
mod snapshot;
mod state;
mod winit;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
use tracing::{error, info};

use crate::state::State;

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => run(),
        Some(Cmd::Reload) => remote("reload", ""),
        Some(Cmd::Quit) => remote("quit", ""),
        Some(Cmd::Eval { code }) => remote("eval", &code),
    }
}

fn remote(cmd: &str, payload: &str) -> ExitCode {
    match ipc::send(cmd, payload) {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,smithay=warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        error!("running from a TTY is not supported yet; start mywm inside a Wayland session");
        return ExitCode::FAILURE;
    }

    match run_nested() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run_nested() -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let mut state = State::new(&mut event_loop, display, true);

    // connects to the host compositor, so it must run before WAYLAND_DISPLAY changes
    winit::init(&mut event_loop, &mut state)?;

    let socket = ipc::socket_path(&state.socket_name);
    ipc::listen(&state.loop_handle, &socket)?;

    // Safety: set before anything is spawned; no other thread reads the environment
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
        std::env::set_var("MYWM_SOCKET", &socket);
        // there is no XWayland yet; X11 apps would open on the host
        std::env::remove_var("DISPLAY");
    }
    info!("listening on WAYLAND_DISPLAY={}", state.socket_name.to_string_lossy());

    state.load_initial_config();

    event_loop.run(None, &mut state, |_| {})?;

    drop(state);
    let _ = std::fs::remove_file(&socket);
    Ok(())
}
