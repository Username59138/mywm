# MyWM

A Wayland compositor where your Lua config *is* the window manager.

The core (Rust, [Smithay](https://github.com/Smithay/smithay)) only knows
clients, outputs, input and a scene graph. Workspaces, layouts, borders,
focus rules, keybinds and animations are plain Lua in `init.lua`.

## Status

Stage 1: runs nested in another Wayland compositor (winit backend).

## Run

```sh
cargo build
./target/debug/mywm          # opens as a window inside your current session
```

Inside MyWM `$MYWM_SOCKET` is set; from outside, the only running session is
used.

```sh
mywm reload                  # reload the config; a broken one keeps the old running
mywm eval 'return #wm.clients()'
mywm quit
```

Ctrl+Alt+Backspace always quits, whatever the config does.

## Config

Searched in order: `~/.config/mywm/init.lua`, `/etc/xdg/mywm/init.lua`, then
the built-in [config/init.lua](config/init.lua) (master-stack tiling with 9
workspaces). Copy that file to start your own. When nested, it uses Alt
instead of Super, since the host compositor keeps Super combos.

Key ideas of the API:

- A window is a scene node (`c.node`). It is invisible until the config adds
  it to the scene, and after the app closes it, the node keeps the last frame
  until the config destroys it, so close animations are up to the config.
- Events, keys, modifiers and easings are constants in strict tables:
  `wm.events.client.nwe` is an error with a suggestion, not a silent no-op.
- `node:animate{}` is declared in Lua and interpolated in Rust.

```lua
wm.on(wm.events.client.ready, function(c)
  local frame = wm.scene.group {}
  frame:add(c.node)
  wm.scene.root:add(frame)  -- from here on the window is visible
  frame.opacity = 0
  frame:animate { opacity = 1, duration = 0.2 }
  wm.input.focus(c)
end)
```

## Ideas

- `mywm lsp`: generate `mywm.meta.lua` type annotations (constants dumped
  from the real prelude, so they never drift) plus a LuaLS config, then exec
  `lua-language-server` with it. Editors get completion, hover docs and typo
  warnings for `wm.*`. Later it could proxy LSP and add `mywm check`
  diagnostics on save.

## Layout

| File | What |
|---|---|
| `src/scene.rs` | scene graph, animations, draw list, hit testing |
| `src/lua/mod.rs` | Lua state, watchdog, nodes as userdata, raw `core` functions |
| `src/lua/prelude.lua` | the `wm` API built on `core` |
| `src/state.rs` | compositor state, config loading and reloading |
| `src/handlers.rs` | Wayland protocol handlers, client lifecycle |
| `src/input.rs` | keyboard and pointer, routed through the config |
| `src/snapshot.rs` | last frame of a closing window |
| `src/winit.rs` | nested backend and rendering |
| `src/ipc.rs` | control socket for the CLI |
