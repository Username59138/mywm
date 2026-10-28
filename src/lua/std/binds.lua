-- mywm.binds: key, mouse button and scroll bindings.
--
--   local binds = require("mywm.binds").new()
--   binds:key(MOD, wm.key.t, function() wm.spawn("foot") end)
--   binds:button(MOD, wm.button.left, desk.mouse_move)
--   binds:scroll(MOD, function(step) desk.view_relative(step) end)
--
-- A bound key or button is consumed: the app under it never sees it.

local ev = wm.events

local Binds = {}
Binds.__index = Binds

local M = {}

-- a wheel notch is about 15; touchpads send many small amounts
local SCROLL_STEP = 15

local function check(fname, mods, fn)
  if type(mods) ~= "number" then error(fname .. ": expected modifiers from wm.mod", 3) end
  if type(fn) ~= "function" then error(fname .. ": expected a function", 3) end
end

function M.new()
  local self = setmetatable({ keys = {}, buttons = {}, scrolls = {}, unsub = {} }, Binds)

  self.unsub[#self.unsub + 1] = wm.on(ev.input.key, function(e)
    if not e.pressed then return false end
    local fn = self.keys[e.mods] and self.keys[e.mods][e.key]
    if not fn then return false end
    fn(e)
    return true
  end)

  self.unsub[#self.unsub + 1] = wm.on(ev.input.button, function(e)
    if not e.pressed then return false end
    local fn = self.buttons[e.mods] and self.buttons[e.mods][e.button]
    if not fn then return false end
    return fn(e) ~= false  -- a handler may return false to let the click through
  end)

  return self
end

function Binds:key(mods, key, fn)
  check("binds:key", mods, fn)
  if type(key) ~= "number" then error("binds:key: expected a key from wm.key", 2) end
  self.keys[mods] = self.keys[mods] or {}
  self.keys[mods][key] = fn
end

-- fn(e) gets the input.button event; return false to pass the click on
function Binds:button(mods, button, fn)
  check("binds:button", mods, fn)
  if type(button) ~= "number" then error("binds:button: expected a button from wm.button", 2) end
  self.buttons[mods] = self.buttons[mods] or {}
  self.buttons[mods][button] = fn
end

-- fn(step, e): step is 1 (down) or -1 (up), once per wheel notch
function Binds:scroll(mods, fn)
  check("binds:scroll", mods, fn)
  if not next(self.scrolls) then
    -- only subscribed while something is bound: scrolling costs nothing otherwise
    local acc = 0
    self.unsub[#self.unsub + 1] = wm.on(ev.input.scroll, function(e)
      local f = self.scrolls[e.mods]
      if not f then acc = 0; return false end
      acc = acc + e.dy
      while math.abs(acc) >= SCROLL_STEP do
        local step = acc > 0 and 1 or -1
        acc = acc - step * SCROLL_STEP
        f(step, e)
      end
      return true
    end)
  end
  self.scrolls[mods] = fn
end

-- remove all handlers of this set of binds
function Binds:destroy()
  for _, u in ipairs(self.unsub) do u() end
  self.unsub, self.keys, self.buttons, self.scrolls = {}, {}, {}, {}
end

return M
