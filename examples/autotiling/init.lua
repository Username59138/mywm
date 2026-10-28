-- Autotiling: every new window splits the focused one along its longer
-- side, like the autotiling daemons for sway/i3. Here it is not a daemon
-- reacting to events but simply the layout.

wm.api(1)

local ev, mod, key = wm.events, wm.mod, wm.key
local scene = wm.scene

local GAP      = 12
local BORDER   = 2
local TERMINAL = os.getenv("TERMINAL") or "foot"

local COLOR_ACTIVE   = "#89b4faff"
local COLOR_INACTIVE = "#ffffff22"

local MOD = wm.nested and mod.alt or mod.super

wm.input.keyboard {
  layout  = "us,ru",
  options = "grp:alt_shift_toggle",
}

---- the tree ----
--
-- Each workspace holds a binary tree. A leaf is { client = c }, an inner
-- node is { split = "h" | "v", ratio = 0.5, a = node, b = node }. Every
-- node knows its parent and the rectangle it got in the last layout.

local function replace(ws, old, new)
  local parent = old.parent
  new.parent = parent
  if not parent then
    ws.tree = new
  elseif parent.a == old then
    parent.a = new
  else
    parent.b = new
  end
end

local function first_leaf(n)
  while n and not n.client do n = n.a end
  return n
end

local function leaves(n, out)
  out = out or {}
  if not n then return out end
  if n.client then
    out[#out + 1] = n.client
  else
    leaves(n.a, out)
    leaves(n.b, out)
  end
  return out
end

-- the whole autotiling: split the target along its longer side
local function insert(ws, c)
  local leaf = { client = c }
  c.leaf = leaf
  if not ws.tree then
    ws.tree = leaf
    return
  end
  local target = ws.last and ws.last.leaf or first_leaf(ws.tree)
  local r = target.rect or { w = 1, h = 0 }
  local inner = { split = r.w >= r.h and "h" or "v", ratio = 0.5, a = target, b = leaf }
  replace(ws, target, inner)
  target.parent, leaf.parent = inner, inner
end

-- returns the node that took the window's place
local function remove(ws, c)
  local leaf = c.leaf
  local parent = leaf.parent
  if not parent then
    ws.tree = nil
    return nil
  end
  local sibling = parent.a == leaf and parent.b or parent.a
  replace(ws, parent, sibling)
  return sibling
end

---- layout ----

local function place(c, r)
  c:configure { width = r.w - 2 * BORDER, height = r.h - 2 * BORDER }
  if c.placed then
    c.frame:animate { x = r.x, y = r.y, duration = 0.22, easing = wm.easing.out_cubic }
  else
    c.frame.x, c.frame.y = r.x, r.y
    c.placed = true
  end
end

local function layout(n, r)
  n.rect = r
  if n.client then
    place(n.client, r)
  elseif n.split == "h" then
    local w1 = math.floor((r.w - GAP) * n.ratio)
    layout(n.a, { x = r.x, y = r.y, w = w1, h = r.h })
    layout(n.b, { x = r.x + w1 + GAP, y = r.y, w = r.w - w1 - GAP, h = r.h })
  else
    local h1 = math.floor((r.h - GAP) * n.ratio)
    layout(n.a, { x = r.x, y = r.y, w = r.w, h = h1 })
    layout(n.b, { x = r.x, y = r.y + h1 + GAP, w = r.w, h = r.h - h1 - GAP })
  end
end

local function tile(ws)
  local out = wm.outputs()[1]
  if not out or not ws.tree then return end
  layout(ws.tree, { x = out.x + GAP, y = out.y + GAP, w = out.width - 2 * GAP, h = out.height - 2 * GAP })
end

---- workspaces and focus ----

local workspaces = {}
for i = 1, 9 do
  local node = scene.group { visible = i == 1 }
  scene.root:add(node)
  workspaces[i] = { node = node, tree = nil, last = nil }
end

local current = workspaces[1]
local focused = nil

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
    c.ws.last = c  -- the next window on this workspace splits this one
  end
end

---- clients ----

wm.on(ev.client.new, function(c)
  c:set_decoration(wm.decoration.server)
end)

wm.on(ev.client.ready, function(c)
  c.frame  = scene.group {}
  c.border = scene.rect { color = COLOR_INACTIVE }
  c.frame:add(c.border)
  c.frame:add(c.node)
  c.node.x, c.node.y = BORDER, BORDER
  c.border.w, c.border.h = c.width + 2 * BORDER, c.height + 2 * BORDER

  c.ws = current
  current.node:add(c.frame)
  insert(current, c)
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
  if not c.ws then return end
  local ws = c.ws
  local took_place = remove(ws, c)
  if ws.last == c then ws.last = nil end
  tile(ws)
  if focused == c then
    focused = nil
    if ws == current then
      local next_leaf = first_leaf(took_place)
      focus(next_leaf and next_leaf.client)
    end
  end
  c.frame:animate {
    opacity = 0, duration = 0.18, easing = wm.easing.out_cubic,
    done = function() c.frame:destroy() end,
  }
end)

wm.on(ev.output.change, function() tile(current) end)

---- actions ----

local function view(i)
  local ws = workspaces[i]
  if ws == current then return end
  current.node.visible = false
  ws.node.visible = true
  ws.node.opacity = 0
  ws.node:animate { opacity = 1, duration = 0.15 }
  current = ws
  tile(ws)
  focus(ws.last or (first_leaf(ws.tree) or {}).client)
end

local function send(i)
  local c, to = focused, workspaces[i]
  if not c or c.ws == to then return end
  local from = c.ws
  local took_place = remove(from, c)
  if from.last == c then from.last = nil end
  to.node:add(c.frame)
  c.ws = to
  insert(to, c)
  to.last = c
  tile(from)
  tile(to)
  local next_leaf = first_leaf(took_place)
  focus(next_leaf and next_leaf.client)
end

local function cycle(step)
  local cs = leaves(current.tree)
  if #cs == 0 then return end
  local i = 1
  for j, c in ipairs(cs) do
    if c == focused then i = j end
  end
  focus(cs[(i - 1 + step) % #cs + 1])
end

-- flip the split the focused window is in between side by side and stacked
local function rotate()
  local p = focused and focused.leaf.parent
  if not p then return end
  p.split = p.split == "h" and "v" or "h"
  tile(current)
end

local function resize(delta)
  local p = focused and focused.leaf.parent
  if not p then return end
  -- growing the focused side means moving the divider towards the other one
  local d = p.a == focused.leaf and delta or -delta
  p.ratio = math.min(0.9, math.max(0.1, p.ratio + d))
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
    return true
  end
  return false
end)

wm.on(ev.input.button, function(e)
  if e.pressed then
    local c = wm.client_at(e.x, e.y)
    if c and c ~= focused then focus(c) end
  end
  return false
end)

bind(MOD,             key.t,     function() wm.spawn(TERMINAL) end)
bind(MOD,             key.q,     function() if focused then focused:close() end end)
bind(MOD,             key.j,     function() cycle(1) end)
bind(MOD,             key.k,     function() cycle(-1) end)
bind(MOD,             key.r,     rotate)
bind(MOD,             key.equal, function() resize(0.05) end)
bind(MOD,             key.minus, function() resize(-0.05) end)
bind(MOD | mod.shift, key.r,     wm.reload)
bind(MOD | mod.shift, key.e,     wm.quit)

for i = 1, 9 do
  bind(MOD,             key.digit(i), function() view(i) end)
  bind(MOD | mod.shift, key.digit(i), function() send(i) end)
end
