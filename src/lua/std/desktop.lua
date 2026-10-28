-- mywm.desktop: a complete tiling window manager written on the wm API.
--
--   local desk = require("mywm.desktop").new {
--     workspaces = 9,
--     layouts = { layouts.tile(), layouts.dwindle(), layouts.monocle() },
--   }
--
-- Workspaces belong to no output until one shows them; every output shows
-- one. Windows tile, float (dialogs, rules, Mod+drag) or go fullscreen.
-- Bars, wallpapers and launchers (layer surfaces) go where they ask, in
-- their layer; windows keep clear of bars, a fullscreen window hides the
-- bars of its output, and a launcher gets the keyboard while it is open.
-- Everything here is ordinary Lua: copy this file into your config and
-- change whatever you like.

local layouts = require("mywm.layouts")

local ev, scene = wm.events, wm.scene

local M = {}

local defaults = {
  workspaces = 9,          -- a count, or a list of names
  layouts = nil,           -- default: tile, dwindle, grid, monocle
  gap = 10,                -- between windows
  outer_gap = nil,         -- around them; default: gap
  reserve = { top = 0, bottom = 0, left = 0, right = 0 },  -- room on top of what bars keep
  border = { width = 2, active = "#89b4faff", inactive = "#ffffff22" },
  animate = { move = 0.22, open = 0.2, close = 0.18, workspace = 0.25, easing = "out_cubic" },
  workspace_animation = "auto", -- "slide", "fade", "none"; auto = slide with one output
  new_window = "master",   -- where a new tiled window goes: "master", "after" (the focused one), "end"
  focus_follows_mouse = false,
  focus_on_click = true,
  server_decorations = true,
  float = nil,             -- function(c) -> bool; default: dialogs, fixed-size windows
  rules = {},              -- { { app_id = "pattern", title = "pattern", float = true, workspace = 2, size = { w, h }, fullscreen = true } }
  on_focus = nil,          -- function(c)
  on_workspace = nil,      -- function(ws, output_name)
}

local function fill(opts, defs)
  local out = {}
  for k, v in pairs(defs) do out[k] = v end
  for k, v in pairs(opts or {}) do
    if defs[k] == nil and k ~= "layouts" and k ~= "outer_gap" and k ~= "float" and k ~= "on_focus"
        and k ~= "on_workspace" then
      error("mywm.desktop: unknown option '" .. tostring(k) .. "'", 3)
    end
    if type(v) == "table" and type(defs[k]) == "table" and k ~= "rules" then
      out[k] = fill(v, defs[k])
    else
      out[k] = v
    end
  end
  return out
end

local function index(list, item)
  for i, v in ipairs(list) do if v == item then return i end end
end

local function remove(list, item)
  local i = index(list, item)
  if i then table.remove(list, i) end
end

local function clamp(v, lo, hi) return math.max(lo, math.min(hi, v)) end

local function default_float(c)
  local t = c.window_type
  return c.parent ~= nil or c.fixed_size
      or t == "dialog" or t == "utility" or t == "splash" or t == "toolbar"
end

function M.new(opts)
  local o = fill(opts, defaults)
  o.layouts = o.layouts or { layouts.tile(), layouts.dwindle(), layouts.grid(), layouts.monocle() }
  o.outer_gap = o.outer_gap or o.gap
  o.float = o.float or default_float
  local B = o.border.width
  local A = o.animate
  local easing = A and wm.easing[A.easing or "out_cubic"]

  local self = {}
  local workspaces = {}   -- by index
  local outputs = {}      -- name -> { name, ws }
  local focused = nil
  local drag = nil        -- a window being moved or resized with the mouse

  -- bottom to top: wallpapers, bottom panels, the workspaces, bars, overlays
  local layer_groups = {}
  local root = scene.group {}
  for _, name in ipairs { "background", "bottom", "workspaces", "top", "overlay" } do
    local g = name == "workspaces" and root or scene.group {}
    scene.root:add(g)
    layer_groups[name] = g
  end
  local kbd_layer = nil   -- a layer surface that has the keyboard
  local grab = nil        -- one that holds it exclusively (a launcher)

  local names = o.workspaces
  if type(names) == "number" then
    names = {}
    for i = 1, o.workspaces do names[i] = tostring(i) end
  end
  for i, name in ipairs(names) do
    local node = scene.group { visible = false }
    local ws = {
      index = i, name = name, clients = {}, node = node,
      tiled = scene.group {}, floating = scene.group {}, full = scene.group {},
      layout = 1, output = nil, last = nil,
    }
    node:add(ws.tiled)
    node:add(ws.floating)
    node:add(ws.full)
    root:add(node)
    workspaces[i] = ws
  end

  ---- geometry ----

  local function output_info(name)
    for _, out in ipairs(wm.outputs()) do
      if out.name == name then return out end
    end
  end

  -- the output rect, and the rect windows tile in
  local function areas(name)
    local out = output_info(name)
    if not out then return end
    local r, g = o.reserve, o.outer_gap
    local u = out.usable or { x = out.x, y = out.y, width = out.width, height = out.height }
    local full = { x = out.x, y = out.y, w = out.width, h = out.height }
    local work = {
      x = u.x + r.left + g, y = u.y + r.top + g,
      w = u.width - r.left - r.right - 2 * g, h = u.height - r.top - r.bottom - 2 * g,
    }
    return full, work
  end

  local function output_at(x, y)
    for _, out in ipairs(wm.outputs()) do
      if x >= out.x and x < out.x + out.width and y >= out.y and y < out.y + out.height then
        return out.name
      end
    end
  end

  -- the output the user works on: the focused window's, else the pointer's
  local function focused_output()
    if focused and focused.ws.output then return focused.ws.output end
    local name = output_at(wm.input.pointer())
    if name and outputs[name] then return name end
    return next(outputs)
  end

  local function current()
    local name = focused_output()
    return name and outputs[name].ws
  end

  local function alive(c)
    return c and not c.closed and c.ws ~= nil
  end

  local function is_floating(c)
    return c.floating and not c.fullscreen and not c.maximized
  end

  local function tiled(ws)
    local out = {}
    for _, c in ipairs(ws.clients) do
      if not c.floating and not c.fullscreen and not c.maximized then out[#out + 1] = c end
    end
    return out
  end

  ---- placing windows ----

  -- outer rect (border included) -> the window's frame
  local function place(c, rect, border)
    local b = border and B or 0
    c.border.visible = b > 0
    c.node.x, c.node.y = b, b
    c:configure { width = math.max(1, rect.w - 2 * b), height = math.max(1, rect.h - 2 * b) }
    if c.placed and A and A.move then
      c.frame:animate { x = rect.x, y = rect.y, duration = A.move, easing = easing }
    else
      c.frame.x, c.frame.y = rect.x, rect.y
      c.placed = true
    end
  end

  local function group_of(c)
    if c.fullscreen then return c.ws.full end
    if is_floating(c) then return c.ws.floating end
    return c.ws.tiled
  end

  local function raise(c)
    group_of(c):add(c.frame)
  end

  -- bars in the top layer go away while their output shows a fullscreen
  -- window, and come back after
  local function update_layers()
    local full_on = {}
    for name, out in pairs(outputs) do
      for _, c in ipairs(out.ws and out.ws.clients or {}) do
        if c.fullscreen and not c.closed then full_on[name] = true end
      end
    end
    for _, l in ipairs(wm.layers()) do
      if l.layer == "top" then l.node.visible = not full_on[l.output] end
    end
  end

  local function arrange(ws)
    if not ws.output then return end
    local full, work = areas(ws.output)
    if not full then return end

    -- floating windows keep their place relative to the output
    if ws.origin and (ws.origin.x ~= full.x or ws.origin.y ~= full.y) then
      for _, c in ipairs(ws.clients) do
        if c.geom then
          c.geom.x = c.geom.x + full.x - ws.origin.x
          c.geom.y = c.geom.y + full.y - ws.origin.y
        end
      end
    end
    ws.origin = { x = full.x, y = full.y }

    local layout = o.layouts[ws.layout]
    local params = {
      ratio = ws.ratio or layout.ratio or 0.5,
      nmaster = ws.nmaster or layout.nmaster or 1,
      gap = o.gap,
    }
    local list = tiled(ws)
    local rects = layout.arrange(#list, work, params)
    for i, c in ipairs(list) do
      group_of(c):add(c.frame)
      if rects[i] then place(c, rects[i], true) end
    end
    for _, c in ipairs(ws.clients) do
      if c.fullscreen then
        place(c, full, false)
      elseif c.maximized then
        place(c, work, true)
      elseif c.floating and c.geom then
        place(c, c.geom, true)
      end
    end
    -- in monocle the focused window must be the one on top
    if focused and focused.ws == ws and not is_floating(focused) then raise(focused) end
    update_layers()
  end

  local function arrange_all()
    for _, ws in ipairs(workspaces) do arrange(ws) end
  end

  ---- focus ----

  local function paint(c, active)
    if c.border then c.border.color = active and o.border.active or o.border.inactive end
  end

  local function focus(c)
    if not alive(c) then c = nil end
    if focused and focused ~= c and alive(focused) then
      focused:set_activated(false)
      paint(focused, false)
    end
    focused = c
    -- an open launcher keeps the keyboard; the window gets it after
    if not grab then
      kbd_layer = nil
      wm.input.focus(c)
    end
    if c then
      c:set_activated(true)
      paint(c, true)
      c.ws.last = c
      raise(c)
    end
    if o.on_focus then o.on_focus(c) end
  end

  -- what gets the focus on a workspace that lost it
  local function refocus(ws)
    if ws and alive(ws.last) and ws.last.ws == ws then return focus(ws.last) end
    focus(ws and ws.clients[1])
  end

  ---- workspaces on outputs ----

  local function slide()
    local mode = o.workspace_animation
    if mode == "auto" then mode = #wm.outputs() == 1 and "slide" or "fade" end
    if not (A and A.workspace) then mode = "none" end
    return mode
  end

  -- dir: 1 = the new workspace comes from the right, -1 from the left
  local function show(ws, name, dir)
    ws.output = name
    outputs[name].ws = ws
    ws.node.visible = true
    arrange(ws)
    local mode = slide()
    if mode == "slide" and dir ~= 0 then
      local full = areas(name)
      ws.node.x = dir * full.w
      ws.node:animate { x = 0, duration = A.workspace, easing = easing }
    elseif mode == "fade" and dir ~= 0 then
      ws.node.x = 0
      ws.node.opacity = 0
      ws.node:animate { opacity = 1, duration = A.workspace, easing = easing }
    else
      ws.node.x, ws.node.opacity = 0, 1
    end
    if o.on_workspace then o.on_workspace(ws, name) end
  end

  local function hide(ws, dir)
    local name = ws.output
    ws.output = nil
    local mode = slide()
    local function gone()
      if not ws.output then
        ws.node.visible = false
        ws.node.x, ws.node.opacity = 0, 1
      end
    end
    if mode == "slide" and dir ~= 0 and name then
      local full = areas(name)
      ws.node:animate { x = -dir * full.w, duration = A.workspace, easing = easing, done = gone }
    else
      gone()
    end
  end

  -- show workspace i on the focused output; if another output shows it,
  -- the two outputs swap workspaces
  function self.view(i)
    local target = workspaces[i]
    local name = focused_output()
    if not target or not name then return end
    local cur = outputs[name].ws
    if target == cur then return end
    if target.output then
      local other = target.output
      target.output = nil
      show(cur, other, 0)
    else
      hide(cur, target.index > cur.index and 1 or -1)
    end
    show(target, name, target.index > cur.index and 1 or -1)
    refocus(target)
  end

  -- the next (1) or previous (-1) workspace, wrapping around
  function self.view_relative(step)
    local cur = current()
    if not cur then return end
    self.view((cur.index - 1 + step) % #workspaces + 1)
  end

  -- move the focused window to workspace i
  function self.send(i, c)
    c = c or focused
    local to = workspaces[i]
    if not alive(c) or not to or c.ws == to then return end
    local from = c.ws
    remove(from.clients, c)
    table.insert(to.clients, 1, c)
    c.ws = to
    if from.last == c then from.last = nil end
    if not to.output then c.placed = false end  -- no flight across the screen later
    group_of(c):add(c.frame)
    arrange(from)
    arrange(to)
    if focused == c then refocus(current()) end
  end

  ---- layouts ----

  function self.layout(step)
    local ws = current()
    if not ws then return end
    ws.layout = (ws.layout - 1 + (step or 1)) % #o.layouts + 1
    arrange(ws)
    wm.notify("layout: " .. o.layouts[ws.layout].name, { timeout = 1 })
  end

  function self.ratio(delta)
    local ws = current()
    if not ws then return end
    local layout = o.layouts[ws.layout]
    ws.ratio = clamp((ws.ratio or layout.ratio or 0.5) + delta, 0.1, 0.9)
    arrange(ws)
  end

  function self.nmaster(delta)
    local ws = current()
    if not ws then return end
    local layout = o.layouts[ws.layout]
    ws.nmaster = math.max(0, (ws.nmaster or layout.nmaster or 1) + delta)
    arrange(ws)
  end

  ---- the focused window ----

  function self.focused() return focused end
  function self.current() return current() end
  function self.workspaces() return workspaces end
  self.focus = focus
  self.arrange = arrange_all

  function self.close(c)
    c = c or focused
    if alive(c) then c:close() end
  end

  function self.cycle(step)
    local ws = current()
    if not ws or #ws.clients == 0 then return end
    local i = index(ws.clients, focused) or 1
    focus(ws.clients[(i - 1 + step) % #ws.clients + 1])
  end

  -- the focused window becomes the master
  function self.zoom()
    local c = focused
    if not alive(c) then return end
    local cs = c.ws.clients
    local i = index(cs, c)
    if i == 1 then
      -- already the master: swap with the next one
      if cs[2] then cs[1], cs[2] = cs[2], cs[1] end
    else
      table.insert(cs, 1, table.remove(cs, i))
    end
    arrange(c.ws)
  end

  local function center(c)
    local w, h = c.width + 2 * B, c.height + 2 * B
    return c.frame.x + w / 2, c.frame.y + h / 2
  end

  -- the nearest visible window in a direction: "left", "right", "up", "down"
  local function neighbor(dir)
    if dir ~= "left" and dir ~= "right" and dir ~= "up" and dir ~= "down" then
      error("expected a direction: left, right, up or down", 3)
    end
    if not alive(focused) then return end
    local fx, fy = center(focused)
    local best, best_score
    for _, ws in ipairs(workspaces) do
      if ws.output then
        for _, c in ipairs(ws.clients) do
          if c ~= focused and alive(c) then
            local x, y = center(c)
            local dx, dy = x - fx, y - fy
            local along, across = ({ left = -dx, right = dx, up = -dy, down = dy })[dir], 0
            across = (dir == "left" or dir == "right") and math.abs(dy) or math.abs(dx)
            if along > 0 then
              local score = along + 2 * across
              if not best_score or score < best_score then best, best_score = c, score end
            end
          end
        end
      end
    end
    return best
  end

  function self.focus_dir(dir)
    local c = neighbor(dir)
    if c then focus(c) end
  end

  -- swap the focused window with its neighbor on the same workspace
  function self.swap_dir(dir)
    local c, other = focused, neighbor(dir)
    if not other or other.ws ~= c.ws then return end
    local cs = c.ws.clients
    local i, j = index(cs, c), index(cs, other)
    cs[i], cs[j] = cs[j], cs[i]
    arrange(c.ws)
  end

  local function default_geom(c)
    local full = areas(c.ws.output or focused_output())
    if not full then return { x = 0, y = 0, w = c.width + 2 * B, h = c.height + 2 * B } end
    local w = math.min(c.width + 2 * B, math.floor(full.w * 0.9))
    local h = math.min(c.height + 2 * B, math.floor(full.h * 0.9))
    -- a dialog opens over its window, anything else in the middle
    local p = c.parent
    local cx, cy = full.x + full.w / 2, full.y + full.h / 2
    if p and p.frame and p.ws and p.ws.output then cx, cy = center(p) end
    return { x = math.floor(cx - w / 2), y = math.floor(cy - h / 2), w = w, h = h }
  end

  function self.toggle_floating(c)
    c = c or focused
    if not alive(c) then return end
    c.floating = not c.floating
    if c.floating and not c.geom then
      -- a tiled window is usually big: float it smaller, in the middle
      local _, work = areas(c.ws.output or focused_output())
      local w, h = c.width + 2 * B, c.height + 2 * B
      if work then
        w, h = math.min(w, math.floor(work.w * 0.6)), math.min(h, math.floor(work.h * 0.6))
        c.geom = { x = work.x + (work.w - w) // 2, y = work.y + (work.h - h) // 2, w = w, h = h }
      else
        c.geom = { x = c.frame.x, y = c.frame.y, w = w, h = h }
      end
    end
    c:set_tiled(not c.floating)
    group_of(c):add(c.frame)
    arrange(c.ws)
  end

  function self.toggle_fullscreen(c)
    c = c or focused
    if not alive(c) then return end
    c:set_fullscreen(not c.fullscreen)
    group_of(c):add(c.frame)
    arrange(c.ws)
  end

  function self.toggle_maximized(c)
    c = c or focused
    if not alive(c) then return end
    c:set_maximized(not c.maximized)
    group_of(c):add(c.frame)
    arrange(c.ws)
  end

  ---- moving and resizing with the mouse ----

  local stop_drag_frame

  local function apply_drag()
    local d = drag
    if not d or not alive(d.c) then return end
    local x, y = wm.input.pointer()
    local dx, dy = x - d.px, y - d.py
    local g, s = d.c.geom, d.start
    if d.mode == "move" then
      g.x, g.y = s.x + dx, s.y + dy
      d.c.frame.x, d.c.frame.y = g.x, g.y
    else
      local min = 60
      if d.edges.left then
        local w = math.max(min, s.w - dx)
        g.x, g.w = s.x + s.w - w, w
      elseif d.edges.right then
        g.w = math.max(min, s.w + dx)
      end
      if d.edges.top then
        local h = math.max(min, s.h - dy)
        g.y, g.h = s.y + s.h - h, h
      elseif d.edges.bottom then
        g.h = math.max(min, s.h + dy)
      end
      d.c.frame.x, d.c.frame.y = g.x, g.y
      d.c:configure { width = g.w - 2 * B, height = g.h - 2 * B }
    end
  end

  local function end_drag()
    drag = nil
    if stop_drag_frame then stop_drag_frame(); stop_drag_frame = nil end
  end

  -- mode: "move" or "resize"; edges: which edges a resize moves (default:
  -- the ones nearest to the pointer)
  local function start_drag(c, mode, button, edges)
    if not alive(c) or c.fullscreen then return end
    if not c.floating then self.toggle_floating(c) end
    if c.maximized then self.toggle_maximized(c) end
    local x, y = wm.input.pointer()
    if not edges then
      local g = c.geom
      edges = {
        left = x < g.x + g.w / 2, right = x >= g.x + g.w / 2,
        top = y < g.y + g.h / 2, bottom = y >= g.y + g.h / 2,
      }
    end
    drag = {
      c = c, mode = mode, button = button or wm.button.left, edges = edges, px = x, py = y,
      start = { x = c.geom.x, y = c.geom.y, w = c.geom.w, h = c.geom.h },
    }
    focus(c)
    -- apply the latest pointer position once per frame, only while dragging
    if not stop_drag_frame then stop_drag_frame = wm.on(ev.output.frame, apply_drag) end
  end

  -- for binds:button(MOD, wm.button.left, desk.mouse_move)
  function self.mouse_move(e)
    local c = wm.client_at(e.x, e.y)
    if not c then return false end
    start_drag(c, "move", e.button)
  end

  function self.mouse_resize(e)
    local c = wm.client_at(e.x, e.y)
    if not c then return false end
    start_drag(c, "resize", e.button)
  end

  ---- rules ----

  local function rule_for(c)
    local r = {}
    for _, rule in ipairs(o.rules) do
      local ok = true
      if rule.app_id and not (c.app_id or ""):find(rule.app_id) then ok = false end
      if rule.title and not (c.title or ""):find(rule.title) then ok = false end
      if ok then
        for k, v in pairs(rule) do
          if k ~= "app_id" and k ~= "title" then r[k] = v end
        end
      end
    end
    return r
  end

  ---- events ----

  wm.on(ev.output.added, function(out)
    outputs[out.name] = { name = out.name }
    for _, ws in ipairs(workspaces) do
      if not ws.output then
        show(ws, out.name, 0)
        break
      end
    end
  end)

  wm.on(ev.output.removed, function(out)
    local o_ = outputs[out.name]
    outputs[out.name] = nil
    if o_ and o_.ws then hide(o_.ws, 0) end
    if focused and not focused.ws.output then refocus(current()) end
  end)

  wm.on(ev.output.change, arrange_all)

  -- a layer surface goes where it asked, in its layer
  local function place_layer(l)
    l.node.x, l.node.y = l.x, l.y
  end

  wm.on(ev.layer.ready, function(l)
    local g = layer_groups[l.layer]
    if not g then return end
    if l.node.parent ~= g then
      g:add(l.node)
      if A and A.open and (l.layer == "top" or l.layer == "overlay") then
        l.node.opacity = 0
        l.node:animate { opacity = 1, duration = A.open, easing = easing }
      end
    end
    place_layer(l)
    if l.keyboard == "exclusive" and (l.layer == "top" or l.layer == "overlay") then
      grab, kbd_layer = l, l
      wm.input.focus(l)
    end
    update_layers()
  end)

  wm.on(ev.layer.change, place_layer)

  -- the keyboard goes back to the focused window
  wm.on(ev.layer.closed, function(l)
    if grab == l then grab = nil end
    if kbd_layer == l then
      kbd_layer = nil
      if not grab then wm.input.focus(alive(focused) and focused or nil) end
    end
  end)

  wm.on(ev.client.new, function(c)
    if o.server_decorations then c:set_decoration(wm.decoration.server) end
  end)

  wm.on(ev.client.ready, function(c)
    local rule = rule_for(c)
    local ws = workspaces[rule.workspace or 0] or current() or workspaces[1]

    c.frame = scene.group {}
    c.border = scene.rect { color = o.border.inactive }
    c.frame:add(c.border)
    c.frame:add(c.node)
    c.border.w, c.border.h = c.width + 2 * B, c.height + 2 * B
    c.ws = ws

    if rule.float ~= nil then c.floating = rule.float else c.floating = o.float(c) end
    if c.floating then
      c.geom = default_geom(c)
      if rule.size then
        c.geom.w, c.geom.h = rule.size[1] + 2 * B, rule.size[2] + 2 * B
        c:configure { width = rule.size[1], height = rule.size[2] }
      end
    end
    c:set_tiled(not c.floating)
    if rule.fullscreen or c.want_fullscreen then c:set_fullscreen(true) end
    if c.want_maximized then c:set_maximized(true) end

    local pos = 1
    if o.new_window == "end" then
      pos = #ws.clients + 1
    elseif o.new_window == "after" and focused and focused.ws == ws then
      pos = index(ws.clients, focused) + 1
    end
    table.insert(ws.clients, pos, c)
    group_of(c):add(c.frame)
    arrange(ws)

    if A and A.open then
      c.frame.opacity = 0
      c.frame:animate { opacity = 1, duration = A.open, easing = easing }
    end
    if ws.output then focus(c) end
  end)

  wm.on(ev.client.resize, function(c)
    if c.border then c.border.w, c.border.h = c.width + 2 * B, c.height + 2 * B end
  end)

  wm.on(ev.client.closed, function(c)
    local ws = c.ws
    if not ws then return end  -- closed before it was shown
    remove(ws.clients, c)
    c.ws = nil
    if ws.last == c then ws.last = nil end
    if drag and drag.c == c then end_drag() end
    arrange(ws)
    if focused == c then
      focused = nil
      refocus(ws.output and ws or current())
    end
    -- the node still shows the last frame: fade it out, then free it
    if A and A.close then
      c.frame:animate {
        opacity = 0, duration = A.close, easing = easing,
        done = function() c.frame:destroy() end,
      }
    else
      c.frame:destroy()
    end
  end)

  -- a request before the window is shown is kept for client.ready
  wm.on(ev.client.request_fullscreen, function(c, e)
    if not c.ws then c.want_fullscreen = e.on; return end
    if alive(c) and c.fullscreen ~= e.on then self.toggle_fullscreen(c) end
  end)

  wm.on(ev.client.request_maximize, function(c, e)
    if not c.ws then c.want_maximized = e.on; return end
    if alive(c) and c.maximized ~= e.on then self.toggle_maximized(c) end
  end)

  -- a title bar drawn by the app was dragged
  wm.on(ev.client.request_move, function(c, e)
    if alive(c) and is_floating(c) then start_drag(c, "move", e.button) end
  end)

  wm.on(ev.client.request_resize, function(c, e)
    if alive(c) and is_floating(c) then start_drag(c, "resize", e.button, e.edges) end
  end)

  wm.on(ev.client.enter, function(c)
    if o.focus_follows_mouse and not drag and alive(c) and c ~= focused then focus(c) end
  end)

  wm.on(ev.input.button, function(e)
    if drag and not e.pressed and e.button == drag.button then
      apply_drag()
      end_drag()
      -- not consumed: a drag the app started needs its release (one that a
      -- bind started is swallowed by the core anyway)
      return false
    end
    if e.pressed and o.focus_on_click and not grab then
      -- a bar with a text field takes the keyboard when clicked
      local l = wm.layer_at(e.x, e.y)
      if l and l.keyboard ~= "none" and kbd_layer ~= l then
        kbd_layer = l
        wm.input.focus(l)
      end
      -- a window takes it back, also when it was focused already
      local c = wm.client_at(e.x, e.y)
      if c then focus(c) end
    end
    return false
  end)

  return self
end

return M
