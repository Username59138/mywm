//! Where configs live: the lookup order, the config chosen with
//! `mywm set config`, and the files autoreload watches.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

pub enum ConfigSource {
    File(PathBuf),
    Embedded,
}

impl ConfigSource {
    pub fn describe(&self) -> String {
        match self {
            ConfigSource::File(p) => p.display().to_string(),
            ConfigSource::Embedded => "built-in default config".into(),
        }
    }

    pub fn dir(&self) -> PathBuf {
        match self {
            ConfigSource::File(p) => p.parent().map(PathBuf::from).unwrap_or_default(),
            ConfigSource::Embedded => user_config_dir(),
        }
    }
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback))
}

pub fn user_config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("mywm")
}

/// Where a session started from a TTY writes its log.
pub fn log_file() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state").join("mywm").join("mywm.log")
}

fn state_file() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state").join("mywm").join("config")
}

/// The config saved with `mywm set config` (its init.lua).
pub fn saved() -> Option<PathBuf> {
    let text = std::fs::read_to_string(state_file()).ok()?;
    let path = PathBuf::from(text.trim());
    (!text.trim().is_empty()).then_some(path)
}

pub fn save(init: &Path) -> Result<(), String> {
    let file = state_file();
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(&file, format!("{}\n", init.display())).map_err(|e| format!("cannot write {}: {e}", file.display()))
}

/// The init.lua of a config given as a directory (or as the file itself).
pub fn resolve(arg: &Path) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(arg).map_err(|e| format!("{}: {e}", arg.display()))?;
    let init = if path.is_dir() { path.join("init.lua") } else { path };
    if init.is_file() {
        Ok(init)
    } else {
        Err(format!("{} has no init.lua", arg.display()))
    }
}

/// Configs in the order they are tried: the chosen one, the user's, the
/// system one, the one built into the binary.
pub fn candidates(chosen: Option<&Path>) -> Vec<ConfigSource> {
    let mut out = Vec::new();
    let defaults = [user_config_dir().join("init.lua"), PathBuf::from("/etc/xdg/mywm/init.lua")];
    for path in chosen.map(Path::to_path_buf).into_iter().chain(defaults) {
        if path.is_file() && !out.iter().any(|s| matches!(s, ConfigSource::File(p) if *p == path)) {
            out.push(ConfigSource::File(path));
        }
    }
    out.push(ConfigSource::Embedded);
    out
}

/// A fingerprint of the Lua files under a config directory: changes when a
/// file is edited, added or removed.
pub fn stamp(dir: &Path) -> (Option<SystemTime>, usize) {
    fn walk(dir: &Path, depth: u32, newest: &mut Option<SystemTime>, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if depth < 4 {
                    walk(&path, depth + 1, newest, count);
                }
            } else if path.extension().is_some_and(|e| e == "lua") {
                *count += 1;
                if let Ok(m) = entry.metadata().and_then(|m| m.modified()) {
                    *newest = Some(newest.map_or(m, |n| n.max(m)));
                }
            }
        }
    }
    let (mut newest, mut count) = (None, 0);
    walk(dir, 0, &mut newest, &mut count);
    (newest, count)
}
