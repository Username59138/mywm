-- The `wm` API, built on the raw functions of the Rust core.
-- Runs before the user's config in every fresh Lua state.

local core, info = ...

---- strict tables: unknown names are errors, with a suggestion ----

local function distance(a, b)
  local la, lb = #a, #b
  if la == 0 then return lb end
  if lb == 0 then return la end
  local prev = {}
  for j = 0, lb do prev[j] = j end
  for i = 1, la do
    local cur = { [0] = i }
    for j = 1, lb do
      local cost = a:sub(i, i) == b:sub(j, j) and 0 or 1
      cur[j] = math.min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + cost)
    end
    prev = cur
  end
  return prev[lb]
end

local function suggest(name, t)
  local best, best_d
  for k in pairs(t) do
    if type(k) == "string" then
      local d = distance(name, k)
      if d <= 3 and (not best_d or d < best_d) then best, best_d = k, d end
    end
  end
  return best
end

-- what: "event", "key", ...; prefix is shown before the name ("client.")
local function strict(what, prefix, t)
  return setmetatable({}, {
    __index = function(_, k)
      local v = t[k]
      if v ~= nil then return v end
      local hint = suggest(tostring(k), t)
      error(("unknown %s '%s%s'%s"):format(what, prefix, tostring(k),
        hint and (", did you mean '" .. prefix .. hint .. "'?") or ""), 2)
    end,
    __newindex = function(_, k)
      error(("cannot set '%s%s': this table is read-only"):format(prefix, tostring(k)), 2)
    end,
    __pairs = function() return next, t, nil end,
  })
end

local function describe(v)
  if type(v) == "string" then return ("string '%s'"):format(v) end
  return type(v)
end

local function check_keys(fname, t, allowed)
  if type(t) ~= "table" then error(fname .. ": expected a table", 3) end
  for k in pairs(t) do
    if not allowed[k] then
      local hint = suggest(tostring(k), allowed)
      error(("%s: unknown field '%s'%s"):format(fname, tostring(k),
        hint and (", did you mean '" .. hint .. "'?") or ""), 3)
    end
  end
end

---- events ----

local event_names = {}
local handlers = {}

local function event(name)
  local e = setmetatable({}, { __tostring = function() return "event " .. name end })
  event_names[e] = name
  handlers[name] = {}
  return e
end

local events = strict("event group", "", {
  client = strict("event", "client.", {
    new    = event("client.new"),
    ready  = event("client.ready"),
    closed = event("client.closed"),
    resize = event("client.resize"),
  }),
  output = strict("event", "output.", {
    change = event("output.change"),
  }),
  input = strict("event", "input.", {
    key    = event("input.key"),
    button = event("input.button"),
  }),
})

local function on(ev, fn)
  local name = event_names[ev]
  if not name then
    error("wm.on: expected an event from wm.events, got " .. describe(ev), 2)
  end
  if type(fn) ~= "function" then
    error("wm.on: expected a handler function, got " .. describe(fn), 2)
  end
  local list = handlers[name]
  list[#list + 1] = fn
  return function()
    for i, h in ipairs(list) do
      if h == fn then table.remove(list, i) return end
    end
  end
end

---- constants ----

local mods = strict("modifier", "", { none = 0, shift = 1, ctrl = 2, alt = 4, super = 8 })

local buttons = strict("button", "", { left = 0x110, right = 0x111, middle = 0x112 })

local keysyms = {
  enter = 0xff0d, space = 0x20, tab = 0xff09, escape = 0xff1b,
  backspace = 0xff08, delete = 0xffff, insert = 0xff63,
  left = 0xff51, up = 0xff52, right = 0xff53, down = 0xff54,
  home = 0xff50, ["end"] = 0xff57, page_up = 0xff55, page_down = 0xff56,
  print = 0xff61,
  comma = 0x2c, period = 0x2e, slash = 0x2f, minus = 0x2d, equal = 0x3d,
  semicolon = 0x3b, apostrophe = 0x27, grave = 0x60, backslash = 0x5c,
  bracket_left = 0x5b, bracket_right = 0x5d,
  volume_up = 0x1008ff13, volume_down = 0x1008ff11,
  mute = 0x1008ff12, mic_mute = 0x1008ffb2,
}
for c = string.byte("a"), string.byte("z") do keysyms[string.char(c)] = c end
for i = 1, 12 do keysyms["f" .. i] = 0xffbe + i - 1 end
keysyms.digit = function(i)
  if type(i) ~= "number" or i < 0 or i > 9 or i % 1 ~= 0 then
    error("wm.key.digit: expected a digit 0..9, got " .. tostring(i), 2)
  end
  return 0x30 + i
end
local keys = strict("key", "", keysyms)

local easing_names = {}
for _, n in ipairs(core.easings()) do easing_names[n] = n end
local easing = strict("easing", "", easing_names)

local decoration = strict("decoration mode", "", { client = "client", server = "server" })

---- clients ----

local clients = {}                                -- id -> client table
local client_ids = setmetatable({}, { __mode = "k" }) -- client table -> id
local live = { title = true, app_id = true, width = true, height = true, closed = true }
local methods = {}

local function id_of(c, fname)
  local id = client_ids[c]
  if not id then error(fname .. ": expected a client, got " .. describe(c), 3) end
  return id
end

local configure_fields = { width = true, height = true }
function methods.configure(c, t)
  local id = id_of(c, "client:configure")
  check_keys("client:configure", t, configure_fields)
  core.client_configure(id, t.width, t.height)
end

function methods.close(c)
  core.client_close(id_of(c, "client:close"))
end

function methods.set_activated(c, on_)
  core.client_activate(id_of(c, "client:set_activated"), on_ and true or false)
end

function methods.set_decoration(c, mode)
  local id = id_of(c, "client:set_decoration")
  if mode ~= "client" and mode ~= "server" then
    error("client:set_decoration: expected wm.decoration.client or wm.decoration.server", 2)
  end
  core.client_decoration(id, mode)
end

local function make_client(id, node)
  local c = {}
  client_ids[c] = id
  return setmetatable(c, {
    __index = function(_, k)
      if k == "node" then return node end
      if k == "id" then return id end
      local m = methods[k]
      if m then return m end
      if live[k] then return core.client_get(id, k) end
      return nil -- the config's own fields that are not set yet
    end,
    __newindex = function(t, k, v)
      if k == "node" or k == "id" or methods[k] or live[k] then
        error("client." .. tostring(k) .. " is read-only", 2)
      end
      rawset(t, k, v)
    end,
    __tostring = function()
      return ("client %d (%s)"):format(id, core.client_get(id, "app_id") or "?")
    end,
  })
end

---- dispatch (called by the core) ----

local function run(name, list, ...)
  local consumed = false
  -- copy, so a handler may unsubscribe while we iterate
  local copy = table.move(list, 1, #list, 1, {})
  for _, h in ipairs(copy) do
    local ok, res = xpcall(h, debug.traceback, ...)
    if not ok then
      core.report(name, tostring(res))
    elseif res == true then
      consumed = true
    end
  end
  return consumed
end

local function dispatch(name, a, b, c, d)
  local list = handlers[name]
  if name == "client.new" then
    clients[a] = make_client(a, b)
    return run(name, list, clients[a])
  elseif name == "client.ready" or name == "client.resize" then
    return run(name, list, clients[a])
  elseif name == "client.closed" then
    local cl = clients[a]
    local r = run(name, list, cl)
    clients[a] = nil
    return r
  elseif name == "input.key" then
    return run(name, list, { key = a, mods = b, pressed = c })
  elseif name == "input.button" then
    return run(name, list, { x = a, y = b, button = c, pressed = d })
  else
    return run(name, list)
  end
end

---- scene ----

local function with_props(node, props, fname)
  if props == nil then return node end
  if type(props) ~= "table" then error(fname .. ": expected a table of properties", 3) end
  for k, v in pairs(props) do
    local ok, err = pcall(function() node[k] = v end)
    if not ok then
      -- keep the message, drop the inner traceback
      local msg = tostring(err):match("^[^\n]*"):gsub("^runtime error: ", "")
      error(fname .. ": " .. msg, 3)
    end
  end
  return node
end

local scene = strict("scene field", "wm.scene.", {
  root  = core.root(),
  group = function(props) return with_props(core.node_new("group"), props, "wm.scene.group") end,
  rect  = function(props) return with_props(core.node_new("rect"), props, "wm.scene.rect") end,
})

---- input ----

local keyboard_fields = {
  layout = true, variant = true, options = true, model = true,
  repeat_rate = true, repeat_delay = true,
}

local input = strict("input field", "wm.input.", {
  focus = function(c)
    if c == nil then core.focus(nil) else core.focus(id_of(c, "wm.input.focus")) end
  end,
  keyboard = function(t)
    check_keys("wm.input.keyboard", t, keyboard_fields)
    core.keyboard(t)
  end,
})

---- the wm table ----

print = function(...)
  local parts = {}
  for i = 1, select("#", ...) do parts[i] = tostring((select(i, ...))) end
  core.log(table.concat(parts, "\t"))
end

wm = strict("wm field", "wm.", {
  api = function(v)
    if v ~= 1 then
      error(("this config needs MyWM API %s, but this MyWM provides API 1"):format(tostring(v)), 2)
    end
  end,

  events     = events,
  mod        = mods,
  key        = keys,
  button     = buttons,
  easing     = easing,
  decoration = decoration,

  on    = on,
  scene = scene,
  input = input,

  clients = function()
    local out = {}
    for _, id in ipairs(core.client_ids()) do
      if clients[id] then out[#out + 1] = clients[id] end
    end
    return out
  end,
  client_at = function(x, y)
    local id = core.client_at(x, y)
    return id and clients[id]
  end,
  outputs = core.outputs,

  spawn  = core.spawn,
  reload = core.reload,
  autoreload = function(on)
    if type(on) ~= "boolean" then error("wm.autoreload: expected true or false", 2) end
    core.autoreload(on)
  end,
  quit   = core.quit,
  time   = core.time,

  config_dir = info.config_dir,
  nested     = info.nested,
})

return dispatch
