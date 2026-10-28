# MyWM

A Wayland compositor where your Lua config *is* the window manager.

The core (Rust, [Smithay](https://github.com/Smithay/smithay)) only knows
clients, outputs, input and a scene graph. Workspaces, layouts, borders,
focus rules, keybinds and animations are plain Lua in `init.lua`.

## Status

Runs nested in another Wayland compositor (winit) or as the real compositor
from a TTY (DRM/KMS, libinput, libseat; one GPU, monitor modes chosen by the
config, explicit sync when the driver supports it), with Xwayland for X11
apps, and layer-shell for bars, wallpapers and launchers (waybar, swaybg,
fuzzel and Quickshell work with the standard desktop). No session lock yet.

X11 windows reach the config like Wayland ones, with `c.x11 = true`; X11
menus and tooltips place themselves and are drawn on top by the core.

## Run

```sh
cargo build
./target/debug/mywm          # inside a Wayland session: opens as a window
                             # on a TTY: takes over the screen
```

On a TTY the log goes to `~/.local/state/mywm/mywm.log`, Ctrl+Alt+F1..F12
switch VTs, and the pointer uses `$XCURSOR_THEME` / `$XCURSOR_SIZE`.

Inside MyWM `$MYWM_SOCKET` is set; from outside, the only running session is
used.

```sh
mywm reload                  # reload the config; a broken one keeps the old running
mywm eval 'return #wm.clients()'
mywm quit

mywm set config <dir>        # switch config (or just save it if MyWM is not running)
mywm set config <dir> -t     # only for this session, keep the saved choice
mywm set config <dir> -r -t  # try a config in a new MyWM window, change nothing
mywm get config              # what runs and why: (saved), (temp, ...), (default)
mywm set autoreload [true|false]  # reload on file changes; no value toggles
mywm get autoreload
mywm check [dir]             # load a config without side effects and report errors
```

The saved choice lives in `$XDG_STATE_HOME/mywm/config`. A config can turn
autoreload on for itself with `wm.autoreload(true)`.

Ctrl+Alt+Backspace always quits, whatever the config does.

## Config

Searched in order: `~/.config/mywm/init.lua`, `/etc/xdg/mywm/init.lua`, then
the built-in [config/init.lua](config/init.lua). Copy that file to start your
own. When nested, the configs use Alt instead of Super, since the host
compositor keeps Super combos.

- [config/init.lua](config/init.lua): the standard library's desktop (see
  below) with a full set of binds, about 100 lines.
- [examples/from-scratch](examples/from-scratch/init.lua): master-stack
  tiling, workspaces, borders and animations on the bare API, no library.
- [examples/autotiling](examples/autotiling/init.lua): every new window
  splits the focused one along its longer side (Mod+R flips a split,
  Mod+= / Mod+- move the divider).

A config can be split into files: `require("foo")` loads `foo.lua` or
`foo/init.lua` next to `init.lua`.

Errors show up on screen: a config that fails to load or reload (the
running one stays), a handler that fails (the same error again and again is
counted, not repeated). A click dismisses a message; a working reload clears
the errors.

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

### Standard library

`require("mywm.std")` (or its parts `mywm.desktop`, `mywm.layouts`,
`mywm.binds`) is a complete window manager written on the same API as any
config, shipped inside the binary. Read it in [src/lua/std](src/lua/std),
copy a module into your config dir to change it.

```lua
local std = require("mywm.std")

local desk = std.desktop.new {
  workspaces = 9,                       -- or { "web", "code", "chat" }
  layouts = { std.layouts.tile { ratio = 0.55 }, std.layouts.dwindle(),
              std.layouts.grid(), std.layouts.monocle() },
  gap = 12,
  border = { width = 2, active = "#89b4faff", inactive = "#ffffff22" },
  focus_follows_mouse = false,
  rules = { { app_id = "^pavucontrol$", float = true, size = { 900, 600 } },
            { app_id = "^firefox$", workspace = 2 } },
}

local binds = std.binds.new()
binds:key(MOD, wm.key.space, function() desk.layout(1) end)
binds:button(MOD, wm.button.left, desk.mouse_move)     -- Mod+drag
binds:scroll(MOD, function(step) desk.view_relative(step) end)
```

- Workspaces on every output (showing one that another output shows swaps
  them), sliding between workspaces with one output, fading with several.
- Layouts are functions from a window count and an area to rects; tile,
  dwindle, grid and monocle are included, writing another is a few lines.
- Floating windows: dialogs, fixed-size windows and rules float by
  themselves; Mod+drag moves or resizes any window (it starts floating), and
  title bars drawn by apps move floating windows.
- Fullscreen and maximize, also when the app asks for them.
- Focus by click, by direction (`desk.focus_dir("left")`), cycling, or
  following the mouse.
- Actions: `view`, `view_relative`, `send`, `layout`, `ratio`, `nmaster`,
  `zoom`, `cycle`, `focus_dir`, `swap_dir`, `toggle_floating`,
  `toggle_fullscreen`, `toggle_maximized`, `close`, `mouse_move`,
  `mouse_resize`; options: `reserve` (room for bars), `new_window`,
  `animate`, `workspace_animation`, `float`, `on_focus`, `on_workspace`.

### Windows

```lua
c.title, c.app_id, c.width, c.height, c.pid, c.x11, c.closed
c.parent                        -- the window a dialog belongs to, or nil
c.min_width, c.max_width, ...   -- size limits the app asks for (0 = none)
c.fixed_size                    -- min == max: a dialog or a splash screen
c.window_type                   -- X11: "dialog", "utility", "splash", ...
c.fullscreen, c.maximized, c.tiled  -- what the config told the app

c:set_fullscreen(true)          -- what the app is told; its place and size
c:set_maximized(true)           -- stay the config's job
c:set_tiled(true)               -- apps with their own title bar square their corners
```

Window events: `client.title` (title or app_id changed), `client.enter` /
`client.leave` (the pointer), and requests the config may grant or ignore:
`client.request_fullscreen` / `request_maximize` (`e.on`),
`request_minimize`, `request_move` (`e.button`, a drag on the app's own title
bar) and `request_resize` (`e.button`, `e.edges.top/bottom/left/right`).
A request made before the window is shown (`foot --fullscreen`) arrives
right after `client.new`.

### Bars, wallpapers, launchers

Layer surfaces work like windows: the app says where it wants to be
(anchors, margins, how much of the screen it keeps for itself), the core
works that out, and nothing is drawn until the config puts `l.node` into
its scene. `o.usable` in `wm.outputs()` is what bars leave free of each
output; `output.change` fires when it changes. The keyboard is the config's
too: a launcher gets it from `wm.input.focus(l)`.

```lua
wm.on(wm.events.layer.ready, function(l)
  -- l.namespace ("waybar", "wallpaper", ...), l.layer ("background", "bottom",
  -- "top", "overlay"), l.output, l.x, l.y, l.width, l.height (where it asked
  -- to be), l.keyboard ("none", "on_demand", "exclusive"), l.exclusive_zone
  layers[l.layer]:add(l.node)
  l.node.x, l.node.y = l.x, l.y
  if l.namespace == "waybar" then        -- or anything else, any animation
    l.node.y = l.y - l.height
    l.node:animate { y = l.y, duration = 0.3 }
  end
  if l.keyboard == "exclusive" then wm.input.focus(l) end
end)
```

`layer.new` (before its first configure), `layer.ready`, `layer.change`
(where it asked to be changed) and `layer.closed` (`e.gone` if destroyed,
not just hidden); `wm.layers()` and `wm.layer_at(x, y)` find them. The
standard desktop puts each in its layer where it asked, keeps windows clear
of bars, hides top-layer bars while their output shows a fullscreen window,
and gives launchers the keyboard while they are open.

### Messages on screen

```lua
wm.notify("saved")                                    -- 5 s
wm.notify("battery low", { level = wm.level.warn })   -- 10 s
wm.notify("no network", { level = wm.level.error, timeout = 0 })  -- until clicked
wm.notify_clear()
```

### Every frame

`output.frame` fires right before each frame of each output, at its refresh
rate, so a handler's changes show in that frame. It costs nothing while no
handler is subscribed.

```lua
local t = 0
wm.on(wm.events.output.frame, function(e)  -- e.output, e.dt (seconds)
  t = t + e.dt
  if focused then focused.border.opacity = 0.7 + 0.3 * math.sin(t * 3) end
end)
```

For a plain move or fade, `node:animate{}` is cheaper: it runs in Rust.

### Mouse

```lua
wm.input.mouse { speed = -0.3, accel = wm.accel.flat, natural_scroll = false, left_handed = false }
wm.input.touchpad { tap = true, natural_scroll = true, disable_while_typing = true }  -- plus the mouse fields

-- e.x, e.y, e.button, e.pressed, e.mods; true = the app never sees the click
wm.on(wm.events.input.button, function(e) ... end)

-- e.x, e.y, e.dx, e.dy, e.mods; always reaches the app
wm.on(wm.events.input.motion, function(e) ... end)

-- e.dx, e.dy (> 0 is down/right, a wheel notch is about 15), e.wheel, e.mods, e.x, e.y
wm.on(wm.events.input.scroll, function(e)
  if e.mods == MOD then view(current_index + (e.dy > 0 and 1 or -1)); return true end
end)

local x, y = wm.input.pointer()
wm.input.warp(x, y)  -- kept on the outputs
```

Mouse and touchpad settings apply on a TTY; nested, the host compositor's
settings are used. A field the config leaves out is the device default, also
after a reload. `input.motion` and `input.scroll` cost nothing while no
handler is subscribed.

### Input for one window

```lua
c:click(x, y)                    -- window coordinates (0, 0 = top left of its content)
c:click(x, y, wm.button.right)   -- false if no part of the window is there
c:type("echo привет\n")          -- any character the configured layouts can type
c:key(wm.key.c, wm.mod.ctrl)     -- one key with modifiers

c:press(10, 10)                  -- hold a button (left by default)...
c:drag(300, 40)                  -- ...move with it held...
c:release()                      -- ...and let go: a text selection, a slider
```

These reach that window whether it is focused or not; the pointer and the
keyboard focus stay where they were, and the modifiers the user holds are
not seen by it. So "which window gets the user's input" is just as much the
config's business as which window gets the focus.

While the config holds a button in a window, the real pointer entering or
leaving that window still reaches the app, which may cut the drag short.

### Environment

```lua
wm.env.XCURSOR_THEME = "Bibata-Modern-Classic"  -- also MyWM's own pointer
wm.env.QT_QPA_PLATFORMTHEME = "qt6ct"
wm.env.FOO = nil                                -- unset for started apps
```

`wm.env` applies to the apps the config starts with `wm.spawn`; MyWM's own
environment stays as it was (`os.getenv` does not see these values). On a
TTY, MyWM's pointer follows `XCURSOR_THEME` and `XCURSOR_SIZE` from it.

### Monitors

`output.added` arrives before a monitor is lit, so the mode is set without
an extra modeset. The built-in config picks the largest resolution at its
highest refresh rate; a fixed setup looks like this:

```lua
wm.on(wm.events.output.added, function(o)
  if o.name == "DP-3" then
    wm.configure_output(o, {
      mode = { width = 2560, height = 1440, refresh = 155 },  -- closest refresh wins
      position = { x = 0, y = 0 },
      scale = 1,
    })
  end
end)
```

`wm.outputs()` lists each output with `mode`, `preferred` and all `modes`.
A mode the monitor does not offer is an error that lists the ones it does.
`wm.configure_output` also works later, for a mode switch at runtime.

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
| `src/lua/std/` | the standard library: `mywm.desktop`, `mywm.layouts`, `mywm.binds` |
| `src/layers.rs` | layer-shell: bars, wallpapers, launchers |
| `src/notify.rs` | messages on screen, with their own text rendering |
| `src/inject.rs` | clicks, held buttons and keys the config sends to a window |
| `src/focus.rs` | what the keyboard focuses: a Wayland surface or an X11 window |
| `src/xwayland.rs` | X11 windows as clients, clipboard between X11 and Wayland |
| `src/udev.rs` | the TTY backend: DRM/KMS, libinput, sessions |
| `src/render.rs` | the scene and the pointer as render elements |
| `src/state.rs` | compositor state, config loading and reloading |
| `src/handlers.rs` | Wayland protocol handlers, client lifecycle |
| `src/input.rs` | keyboard and pointer, routed through the config |
| `src/snapshot.rs` | last frame of a closing window |
| `src/winit.rs` | nested backend and rendering |
| `src/ipc.rs` | control socket for the CLI |

## License

GPL-3.0-or-later, see [LICENSE](LICENSE).
