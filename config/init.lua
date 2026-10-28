-- MyWM default config: master-stack tiling with 9 workspaces.
--
-- Copy it to ~/.config/mywm/init.lua and make it yours. Everything below is
-- plain Lua on top of the core API: the core itself knows nothing about
-- workspaces, layouts, borders, focus rules or keybinds.

wm.api(1)

local ev, mod, key = wm.events, wm.mod, wm.key
local scene = wm.scene

local GAP      = 12
local BORDER   = 2
local TERMINAL = os.getenv("TERMINAL") or "foot"

local COLOR_ACTIVE   = "#89b4faff"
local COLOR_INACTIVE = "#ffffff22"

-- Nested inside another compositor, Super combos belong to the host,
-- so use Alt there.
local MOD = wm.nested and mod.alt or mod.super

wm.input.keyboard {
  layout  = "us,ru",
  options = "grp:alt_shift_toggle",
}

---- helpers ----

local function index(list, item)
  for i, v in ipairs(list) do
    if v == item then return i end
  end
end

local function remove(list, item)
  local i = index(list, item)
  if i then table.remove(list, i) end
end

---- workspaces: a Lua table and a scene group each ----

local workspaces = {}
for i = 1, 9 do
  local node = scene.group { visible = i == 1 }
  scene.root:add(node)
  workspaces[i] = { clients = {}, node = node }
end

local current = workspaces[1]
local focused = nil

---- focus ----

local function focus(c)
  if focused and focused ~= c and not focused.closed then
    focused:set_activated(false)
    focused.border.color = COLOR_INACTIVE
  end
  focused = c
  wm.input.focus(c)
  if c then
    c:set_activated(true)
    c.border.color = COLOR_ACTIVE
  end
end

---- layout: master on the left, stack on the right ----

local function tile(ws)
  local out = wm.outputs()[1]
  local n = #ws.clients
  if not out or n == 0 then return end

  local x, y = out.x + GAP, out.y + GAP
  local w, h = out.width - 2 * GAP, out.height - 2 * GAP
  local mw = n == 1 and w or math.floor((w - GAP) * 0.55)

  for i, c in ipairs(ws.clients) do
    local g
    if i == 1 then
      g = { x = x, y = y, w = mw, h = h }
    else
      local sh = math.floor((h - GAP * (n - 2)) / (n - 1))
      g = { x = x + mw + GAP, y = y + (i - 2) * (sh + GAP), w = w - mw - GAP, h = sh }
    end

    -- the border is a rect behind the window; it follows the size the app
    -- really takes (terminals round it to whole cells), see client.resize
    c:configure { width = g.w - 2 * BORDER, height = g.h - 2 * BORDER }

    if c.placed then
      c.frame:animate { x = g.x, y = g.y, duration = 0.22, easing = wm.easing.out_cubic }
    else
      c.frame.x, c.frame.y = g.x, g.y
      c.placed = true
    end
  end
end

---- clients ----

wm.on(ev.client.new, function(c)
  -- we draw the border; the app does not need its own title bar
  c:set_decoration(wm.decoration.server)
end)

wm.on(ev.client.ready, function(c)
  -- a window shows up only once the config puts its node into the scene
  c.frame  = scene.group {}
  c.border = scene.rect { color = COLOR_INACTIVE }
  c.frame:add(c.border)
  c.frame:add(c.node)
  c.node.x, c.node.y = BORDER, BORDER
  c.border.w, c.border.h = c.width + 2 * BORDER, c.height + 2 * BORDER

  c.ws = current
  current.node:add(c.frame)
  table.insert(current.clients, 1, c)  -- a new window becomes the master
  tile(current)

  c.frame.opacity = 0
  c.frame:animate { opacity = 1, duration = 0.2, easing = wm.easing.out_cubic }
  focus(c)
end)

wm.on(ev.client.resize, function(c)
  if c.border then
    c.border.w, c.border.h = c.width + 2 * BORDER, c.height + 2 * BORDER
  end
end)

wm.on(ev.client.closed, function(c)
  if not c.ws then return end  -- closed before it was ever shown

  remove(c.ws.clients, c)
  tile(c.ws)
  if focused == c then
    focused = nil
    focus(current.clients[1])
  end

  -- the node still holds the last frame: fade it out, then free it
  c.frame:animate {
    opacity = 0, duration = 0.18, easing = wm.easing.out_cubic,
    done = function() c.frame:destroy() end,
  }
end)

wm.on(ev.output.change, function() tile(current) end)

---- workspace actions ----

local function view(i)
  local ws = workspaces[i]
  if ws == current then return end
  current.node.visible = false
  ws.node.visible = true
  ws.node.opacity = 0
  ws.node:animate { opacity = 1, duration = 0.15 }
  current = ws
  tile(ws)  -- the output may have changed while it was hidden
  focus(ws.clients[1])
end

local function send(i)
  local c, to = focused, workspaces[i]
  if not c or c.ws == to then return end
  local from = c.ws
  remove(from.clients, c)
  table.insert(to.clients, 1, c)
  to.node:add(c.frame)  -- add() moves it out of the old workspace
  c.ws = to
  tile(from)
  tile(to)
  focus(current.clients[1])
end

local function cycle(step)
  local cs = current.clients
  if #cs == 0 then return end
  local i = index(cs, focused) or 1
  focus(cs[(i - 1 + step) % #cs + 1])
end

local function zoom()  -- the focused window becomes the master
  local cs = current.clients
  local i = index(cs, focused)
  if not i or i == 1 then return end
  table.insert(cs, 1, table.remove(cs, i))
  tile(current)
end

---- input ----

local binds = {}
local function bind(mods, k, fn)
  binds[mods] = binds[mods] or {}
  binds[mods][k] = fn
end

wm.on(ev.input.key, function(e)
  if not e.pressed then return false end
  local fn = binds[e.mods] and binds[e.mods][e.key]
  if fn then
    fn()
    return true  -- consumed: the focused app never sees it
  end
  return false
end)

-- click to focus
wm.on(ev.input.button, function(e)
  if e.pressed then
    local c = wm.client_at(e.x, e.y)
    if c and c ~= focused then focus(c) end
  end
  return false  -- the click still reaches the app
end)

bind(MOD,             key.t,      function() wm.spawn(TERMINAL) end)
bind(MOD,             key.q,      function() if focused then focused:close() end end)
bind(MOD,             key.j,      function() cycle(1) end)
bind(MOD,             key.k,      function() cycle(-1) end)
bind(MOD,             key.enter,  zoom)
bind(MOD | mod.shift, key.r,      wm.reload)
bind(MOD | mod.shift, key.e,      wm.quit)

for i = 1, 9 do
  bind(MOD,             key.digit(i), function() view(i) end)
  bind(MOD | mod.shift, key.digit(i), function() send(i) end)
end
