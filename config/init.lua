-- MyWM default config, built on the standard library.
--
-- Copy it to ~/.config/mywm/init.lua and make it yours. The window manager
-- itself is require("mywm.desktop"): plain Lua you can read, configure or
-- replace (see examples/from-scratch for one written on the bare API).

wm.api(1)

local std = require("mywm.std")
local layouts = std.layouts
local mod, key, button = wm.mod, wm.key, wm.button

local TERMINAL = os.getenv("TERMINAL") or "foot"

-- Nested inside another compositor, Super combos belong to the host,
-- so use Alt there.
local MOD = wm.nested and mod.alt or mod.super

wm.input.keyboard {
  layout  = "us,ru",
  options = "grp:alt_shift_toggle",
}

---- monitors: the largest resolution at its highest refresh rate ----

wm.on(wm.events.output.added, function(o)
  local best
  for _, m in ipairs(o.modes) do
    local area, best_area = m.width * m.height, best and best.width * best.height or 0
    if area > best_area or (area == best_area and m.refresh > best.refresh) then
      best = m
    end
  end
  if best then wm.configure_output(o, { mode = best }) end
end)

---- the window manager ----

local desk = std.desktop.new {
  workspaces = 9,
  layouts = { layouts.tile { ratio = 0.55 }, layouts.dwindle(), layouts.grid(), layouts.monocle() },
  gap = 12,
  border = { width = 2, active = "#89b4faff", inactive = "#ffffff22" },
  focus_follows_mouse = false,
  rules = {
    -- { app_id = "^pavucontrol$", float = true, size = { 900, 600 } },
    -- { app_id = "^firefox$", workspace = 2 },
  },
}

---- binds ----

local binds = std.binds.new()
local function bind(mods, k, fn) binds:key(mods, k, fn) end

bind(MOD,             key.t,      function() wm.spawn(TERMINAL) end)
bind(MOD,             key.q,      function() desk.close() end)
bind(MOD,             key.j,      function() desk.cycle(1) end)
bind(MOD,             key.k,      function() desk.cycle(-1) end)
bind(MOD,             key.left,   function() desk.focus_dir("left") end)
bind(MOD,             key.right,  function() desk.focus_dir("right") end)
bind(MOD,             key.up,     function() desk.focus_dir("up") end)
bind(MOD,             key.down,   function() desk.focus_dir("down") end)
bind(MOD | mod.shift, key.left,   function() desk.swap_dir("left") end)
bind(MOD | mod.shift, key.right,  function() desk.swap_dir("right") end)
bind(MOD | mod.shift, key.up,     function() desk.swap_dir("up") end)
bind(MOD | mod.shift, key.down,   function() desk.swap_dir("down") end)
bind(MOD,             key.enter,  desk.zoom)
bind(MOD,             key.space,  function() desk.layout(1) end)
bind(MOD,             key.h,      function() desk.ratio(-0.05) end)
bind(MOD,             key.l,      function() desk.ratio(0.05) end)
bind(MOD,             key.comma,  function() desk.nmaster(1) end)
bind(MOD,             key.period, function() desk.nmaster(-1) end)
bind(MOD,             key.v,      function() desk.toggle_floating() end)
bind(MOD,             key.f,      function() desk.toggle_fullscreen() end)
bind(MOD,             key.m,      function() desk.toggle_maximized() end)
bind(MOD,             key.escape, wm.notify_clear)
bind(MOD | mod.shift, key.r,      wm.reload)
bind(MOD | mod.shift, key.e,      wm.quit)

for i = 1, 9 do
  bind(MOD,             key.digit(i), function() desk.view(i) end)
  bind(MOD | mod.shift, key.digit(i), function() desk.send(i) end)
end

-- Mod+drag moves a window (it starts floating), Mod+right drag resizes it
binds:button(MOD, button.left, desk.mouse_move)
binds:button(MOD, button.right, desk.mouse_resize)
-- Mod+wheel goes through the workspaces
binds:scroll(MOD, function(step) desk.view_relative(step) end)
