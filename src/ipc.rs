//! The control socket behind `mywm reload`, `mywm eval` and the like.
//!
//! Protocol: the client writes `<command>\n<payload>` and closes its write
//! side; the server answers `ok\n<text>` or `err\n<text>`.

use std::{
    ffi::OsStr,
    io::{self, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    time::Duration,
};

use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use tracing::warn;

use crate::state::State;

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

pub fn socket_path(wayland_socket: &OsStr) -> PathBuf {
    runtime_dir().join(format!("mywm-{}.sock", wayland_socket.to_string_lossy()))
}

pub fn listen(handle: &LoopHandle<'static, State>, path: &Path) -> io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    handle
        .insert_source(Generic::new(listener, Interest::READ, Mode::Level), |_, listener, state| {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => serve(state, stream),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        warn!("control socket: {e}");
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(())
}

fn serve(state: &mut State, mut stream: UnixStream) {
    let mut request = String::new();
    let read = stream
        .set_nonblocking(false)
        .and_then(|_| stream.set_read_timeout(Some(Duration::from_secs(1))))
        .and_then(|_| stream.read_to_string(&mut request));
    if read.is_err() || request.is_empty() {
        return; // a probe from `find_socket`, or a broken client
    }
    let (cmd, payload) = request.split_once('\n').unwrap_or((&request, ""));

    let reply = match cmd {
        "reload" => state.reload().map(|src| format!("config reloaded ({src})")),
        "quit" => {
            state.loop_signal.stop();
            Ok(String::new())
        }
        "eval" => state.eval(payload),
        other => Err(format!("unknown command '{other}'")),
    };
    let text = match reply {
        Ok(out) => format!("ok\n{out}"),
        Err(e) => format!("err\n{e}"),
    };
    let _ = stream.write_all(text.as_bytes());
}

/// The socket of the session to talk to: $MYWM_SOCKET inside MyWM, or the
/// only running session otherwise.
fn find_socket() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("MYWM_SOCKET") {
        return Ok(p.into());
    }
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(runtime_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("mywm-") && name.ends_with(".sock") && UnixStream::connect(entry.path()).is_ok() {
                found.push(entry.path());
            }
        }
    }
    match found.len() {
        0 => Err("MyWM is not running".into()),
        1 => Ok(found.remove(0)),
        _ => Err(format!(
            "several MyWM sessions are running; set MYWM_SOCKET to one of:\n{}",
            found.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n")
        )),
    }
}

pub fn send(cmd: &str, payload: &str) -> Result<String, String> {
    let path = find_socket()?;
    let mut stream = UnixStream::connect(&path).map_err(|e| format!("cannot connect to {}: {e}", path.display()))?;
    stream
        .write_all(format!("{cmd}\n{payload}").as_bytes())
        .and_then(|_| stream.shutdown(std::net::Shutdown::Write))
        .map_err(|e| e.to_string())?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).map_err(|e| e.to_string())?;
    match reply.split_once('\n') {
        Some(("ok", out)) => Ok(out.to_string()),
        Some(("err", e)) => Err(e.to_string()),
        _ => Err("MyWM closed the connection without an answer".into()),
    }
}
