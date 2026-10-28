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
    title  = event("client.title"),
    enter  = event("client.enter"),
    leave  = event("client.leave"),
    request_fullscreen = event("client.request_fullscreen"),
    request_maximize   = event("client.request_maximize"),
    request_minimize   = event("client.request_minimize"),
    request_move       = event("client.request_move"),
    request_resize     = event("client.request_resize"),
  }),
  output = strict("event", "output.", {
    added   = event("output.added"),
    removed = event("output.removed"),
    change  = event("output.change"),
    frame   = event("output.frame"),
  }),
  layer = strict("event", "layer.", {
    new    = event("layer.new"),
    ready  = event("layer.ready"),
    change = event("layer.change"),
    closed = event("layer.closed"),
  }),
  input = strict("event", "input.", {
    key    = event("input.key"),
    button = event("input.button"),
    motion = event("input.motion"),
    scroll = event("input.scroll"),
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
  core.subscribed(name, true)
  return function()
    for i, h in ipairs(list) do
      if h == fn then
        table.remove(list, i)
        if #list == 0 then core.subscribed(name, false) end
        return
      end
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

local levels = strict("notification level", "", { info = "info", warn = "warn", error = "error" })

local notify_fields = { level = true, timeout = true }
-- wm.notify(text, { level = wm.level.warn, timeout = seconds (0 = until clicked) })
local function notify(text, opts)
  if type(text) ~= "string" then text = tostring(text) end
  opts = opts or {}
  check_keys("wm.notify", opts, notify_fields)
  local level = opts.level or "info"
  if level ~= "info" and level ~= "warn" and level ~= "error" then
    error("wm.notify: level must be wm.level.info, warn or error", 2)
  end
  if opts.timeout ~= nil and type(opts.timeout) ~= "number" then
    error("wm.notify: timeout must be a number of seconds", 2)
  end
  core.notify(text, level, opts.timeout)
end

local accel = strict("accel profile", "", { flat = "flat", adaptive = "adaptive" })

local decoration = strict("decoration mode", "", { client = "client", server = "server" })

---- clients ----

local clients = {}                                -- id -> client table
local client_ids = setmetatable({}, { __mode = "k" }) -- client table -> id
local live = {
  title = true, app_id = true, width = true, height = true, closed = true, x11 = true,
  pid = true, min_width = true, min_height = true, max_width = true, max_height = true,
  fullscreen = true, maximized = true, tiled = true, window_type = true,
}
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

-- strips "runtime error: " and the traceback from a core error
local function rethrow(err, level)
  error(tostring(err):match("^[^\n]*"):gsub("^runtime error: ", ""), level + 1)
end

-- x, y in the window's own coordinates (0, 0 = top left of its content);
-- false if no part of the window is there
function methods.click(c, x, y, button)
  local id = id_of(c, "client:click")
  if type(x) ~= "number" or type(y) ~= "number" then
    error("client:click: expected x and y", 2)
  end
  if button ~= nil and type(button) ~= "number" then
    error("client:click: expected a button from wm.button, got " .. describe(button), 2)
  end
  return core.client_click(id, x, y, button or buttons.left)
end

function methods.type(c, text)
  local id = id_of(c, "client:type")
  if type(text) ~= "string" then error("client:type: expected a string, got " .. describe(text), 2) end
  local ok, err = pcall(core.client_type, id, text)
  if not ok then rethrow(err, 2) end
end

-- a key from wm.key, with wm.mod bits held: c:key(wm.key.c, wm.mod.ctrl)
function methods.key(c, k, m)
  local id = id_of(c, "client:key")
  if type(k) ~= "number" then error("client:key: expected a key from wm.key, got " .. describe(k), 2) end
  if m ~= nil and type(m) ~= "number" then
    error("client:key: expected modifiers from wm.mod, got " .. describe(m), 2)
  end
  local ok, err = pcall(core.client_key, id, k, m)
  if not ok then rethrow(err, 2) end
end

local function state_setter(state)
  local fname = "client:set_" .. state
  return function(c, on_)
    local id = id_of(c, fname)
    if type(on_) ~= "boolean" then error(fname .. ": expected true or false, got " .. describe(on_), 2) end
    core.client_set_state(id, state, on_)
  end
end
-- what the app is told; where and how big the window is stays the config's job
methods.set_fullscreen = state_setter("fullscreen")
methods.set_maximized  = state_setter("maximized")
-- tells apps that draw their own decorations to drop shadows and round corners
methods.set_tiled      = state_setter("tiled")

-- hold a button down at x, y of the window; false if nothing is there
function methods.press(c, x, y, button)
  local id = id_of(c, "client:press")
  if type(x) ~= "number" or type(y) ~= "number" then error("client:press: expected x and y", 2) end
  if button ~= nil and type(button) ~= "number" then
    error("client:press: expected a button from wm.button, got " .. describe(button), 2)
  end
  return core.client_press(id, x, y, button or buttons.left)
end

-- move the held pointer
function methods.drag(c, x, y)
  local id = id_of(c, "client:drag")
  if type(x) ~= "number" or type(y) ~= "number" then error("client:drag: expected x and y", 2) end
  core.client_drag(id, x, y)
end

function methods.release(c, button)
  local id = id_of(c, "client:release")
  if button ~= nil and type(button) ~= "number" then
    error("client:release: expected a button from wm.button, got " .. describe(button), 2)
  end
  core.client_release(id, button or buttons.left)
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
      if k == "parent" then
        local p = core.client_get(id, "parent")
        return p and clients[p]
      end
      if k == "fixed_size" then  -- min == max: a dialog or a splash screen
        local w, h = core.client_get(id, "min_width"), core.client_get(id, "min_height")
        return w > 0 and h > 0 and w == core.client_get(id, "max_width") and h == core.client_get(id, "max_height")
      end
      return nil -- the config's own fields that are not set yet
    end,
    __newindex = function(t, k, v)
      if k == "node" or k == "id" or methods[k] or live[k] or k == "parent" or k == "fixed_size" then
        error("client." .. tostring(k) .. " is read-only", 2)
      end
      rawset(t, k, v)
    end,
    __tostring = function()
      return ("client %d (%s)"):format(id, core.client_get(id, "app_id") or "?")
    end,
  })
end

---- layer surfaces (bars, wallpapers, launchers) ----

local layers = {}                                 -- id -> layer table
local layer_ids = setmetatable({}, { __mode = "k" }) -- layer table -> id
local layer_live = {
  namespace = true, layer = true, output = true, x = true, y = true, width = true, height = true,
  keyboard = true, exclusive_zone = true, closed = true,
}

local function make_layer(id, node)
  local l = {}
  layer_ids[l] = id
  return setmetatable(l, {
    __index = function(_, k)
      if k == "node" then return node end
      if k == "id" then return id end
      if layer_live[k] then return core.layer_get(id, k) end
      return nil
    end,
    __newindex = function(t, k, v)
      if k == "node" or k == "id" or layer_live[k] then
        error("layer." .. tostring(k) .. " is read-only", 2)
      end
      rawset(t, k, v)
    end,
    __tostring = function()
      return ("layer %d (%s)"):format(id, core.layer_get(id, "namespace") or "?")
    end,
  })
end

---- dispatch (called by the core) ----

-- a traceback without MyWM's own frames
local function traceback(msg)
  local out = {}
  for line in debug.traceback(msg, 2):gmatch("[^\n]+") do
    if not (line:find("prelude:", 1, true) or line:find("[C]: in function 'xpcall'", 1, true)
        or line:find("(...tail calls...)", 1, true)) then
      out[#out + 1] = line
    end
  end
  if out[#out] == "stack traceback:" then out[#out] = nil end
  return table.concat(out, "\n")
end

local function run(name, list, ...)
  local consumed = false
  -- copy, so a handler may unsubscribe while we iterate
  local copy = table.move(list, 1, #list, 1, {})
  for _, h in ipairs(copy) do
    local ok, res = xpcall(h, traceback, ...)
    if not ok then
      core.report(name, tostring(res))
    elseif res == true then
      consumed = true
    end
  end
  return consumed
end

local function dispatch(name, a, b, c, d, e, f)
  local list = handlers[name]
  if name == "client.new" then
    clients[a] = make_client(a, b)
    return run(name, list, clients[a])
  elseif name == "client.ready" or name == "client.resize" or name == "client.title"
      or name == "client.enter" or name == "client.leave" or name == "client.request_minimize" then
    if not clients[a] then return false end
    return run(name, list, clients[a])
  elseif name == "client.request_fullscreen" or name == "client.request_maximize" then
    if not clients[a] then return false end
    return run(name, list, clients[a], { on = b })
  elseif name == "client.request_move" then
    if not clients[a] then return false end
    return run(name, list, clients[a], { button = b })
  elseif name == "client.request_resize" then
    if not clients[a] then return false end
    local edges = { top = c & 1 ~= 0, bottom = c & 2 ~= 0, left = c & 4 ~= 0, right = c & 8 ~= 0 }
    return run(name, list, clients[a], { button = b, edges = edges })
  elseif name == "client.closed" then
    local cl = clients[a]
    local r = run(name, list, cl)
    clients[a] = nil
    return r
  elseif name == "layer.new" then
    layers[a] = make_layer(a, b)
    return run(name, list, layers[a])
  elseif name == "layer.ready" or name == "layer.change" then
    if not layers[a] then return false end
    return run(name, list, layers[a])
  elseif name == "layer.closed" then
    local l = layers[a]
    if not l then return false end
    local r = run(name, list, l, { gone = b })
    if b then layers[a] = nil end
    return r
  elseif name == "output.added" then
    local info
    for _, o in ipairs(core.outputs()) do
      if o.name == a then info = o end
    end
    return run(name, list, info or { name = a, modes = {} })
  elseif name == "output.frame" then
    return run(name, list, { output = a, dt = b })
  elseif name == "output.removed" then
    return run(name, list, { name = a })
  elseif name == "input.key" then
    return run(name, list, { key = a, mods = b, pressed = c })
  elseif name == "input.button" then
    return run(name, list, { x = a, y = b, button = c, pressed = d, mods = e })
  elseif name == "input.motion" then
    run(name, list, { x = a, y = b, dx = c, dy = d, mods = e })
    return false  -- motion always reaches the app
  elseif name == "input.scroll" then
    return run(name, list, { x = a, y = b, dx = c, dy = d, mods = e, wheel = f })
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

local pointer_fields = {
  speed = "number", accel = "string", natural_scroll = "boolean", left_handed = "boolean",
}
local touchpad_fields = {
  speed = "number", accel = "string", natural_scroll = "boolean", left_handed = "boolean",
  tap = "boolean", disable_while_typing = "boolean",
}

local function pointer_settings(kind, fields)
  local fname = "wm.input." .. kind
  return function(t)
    check_keys(fname, t, fields)
    for k, v in pairs(t) do
      if type(v) ~= fields[k] then
        error(("%s: %s must be a %s, got %s"):format(fname, k, fields[k], describe(v)), 2)
      end
    end
    if t.accel ~= nil and t.accel ~= "flat" and t.accel ~= "adaptive" then
      error(fname .. ": accel must be wm.accel.flat or wm.accel.adaptive", 2)
    end
    local ok, err = pcall(core.pointer_settings, kind, t)
    if not ok then error(tostring(err):match("^[^\n]*"):gsub("^runtime error: ", ""), 2) end
  end
end

local input = strict("input field", "wm.input.", {
  -- a client, a layer that takes keyboard input, or nil
  focus = function(c)
    if c == nil then core.focus(nil)
    elseif layer_ids[c] then core.focus(layer_ids[c])
    else core.focus(id_of(c, "wm.input.focus")) end
  end,
  keyboard = function(t)
    check_keys("wm.input.keyboard", t, keyboard_fields)
    core.keyboard(t)
  end,
  mouse    = pointer_settings("mouse", pointer_fields),
  touchpad = pointer_settings("touchpad", touchpad_fields),
  -- where the pointer is: x, y
  pointer = core.pointer,
  warp = function(x, y)
    if type(x) ~= "number" or type(y) ~= "number" then
      error("wm.input.warp: expected x and y", 2)
    end
    core.warp(x, y)
  end,
})

---- outputs ----

local output_fields = { mode = true, position = true, scale = true }
local mode_fields = { width = true, height = true, refresh = true }
local position_fields = { x = true, y = true }

-- o: an output from wm.outputs() / output.added, or its name
local function configure_output(o, opts)
  local name = type(o) == "table" and o.name or o
  if type(name) ~= "string" then
    error("wm.configure_output: expected an output or its name, got " .. describe(o), 2)
  end
  check_keys("wm.configure_output", opts, output_fields)
  if opts.mode ~= nil then
    check_keys("wm.configure_output: mode", opts.mode, mode_fields)
    if type(opts.mode.width) ~= "number" or type(opts.mode.height) ~= "number" then
      error("wm.configure_output: mode needs width and height", 2)
    end
  end
  if opts.position ~= nil then
    check_keys("wm.configure_output: position", opts.position, position_fields)
  end
  local ok, err = pcall(core.configure_output, name, opts)
  if not ok then
    error(tostring(err):match("^[^\n]*"):gsub("^runtime error: ", ""), 2)
  end
end

---- environment ----

-- wm.env.NAME reads and sets variables for the apps the config starts;
-- assigning nil unsets one. MyWM's own environment is not touched.
local env = setmetatable({}, {
  __index = function(_, name)
    if type(name) ~= "string" then error("wm.env: variable names are strings", 2) end
    return core.env_get(name)
  end,
  __newindex = function(_, name, value)
    if type(name) ~= "string" then error("wm.env: variable names are strings", 2) end
    if type(value) == "number" then value = tostring(value) end
    if value ~= nil and type(value) ~= "string" then
      error(("wm.env.%s: expected a string, got %s"):format(name, describe(value)), 2)
    end
    local ok, err = pcall(core.env_set, name, value)
    if not ok then error(tostring(err):match("^[^\n]*"):gsub("^runtime error: ", ""), 2) end
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
  accel      = accel,
  level      = levels,

  notify       = notify,
  notify_clear = core.notify_clear,

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
  layers = function()
    local out = {}
    for _, id in ipairs(core.layer_ids()) do
      if layers[id] then out[#out + 1] = layers[id] end
    end
    return out
  end,
  layer_at = function(x, y)
    local id = core.layer_at(x, y)
    return id and layers[id]
  end,
  client_at = function(x, y)
    local id = core.client_at(x, y)
    return id and clients[id]
  end,
  outputs = core.outputs,
  env     = env,
  configure_output = configure_output,

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
