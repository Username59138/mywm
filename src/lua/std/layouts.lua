-- mywm.layouts: tiling layouts as plain functions.
--
-- A layout is a table { name = "...", arrange = function(n, area, p) }.
-- arrange gets the number of tiled windows, the area to fill
-- ({ x, y, w, h }) and the workspace parameters p ({ ratio, nmaster, gap }),
-- and returns one rect per window, in the workspace's window order.
-- Writing your own is just writing such a function.

local M = {}

-- n sizes that add up to total minus the gaps between them
local function parts(total, n, gap)
  local avail = total - gap * (n - 1)
  local sizes = {}
  for i = 1, n do
    sizes[i] = math.floor(avail * i / n) - math.floor(avail * (i - 1) / n)
  end
  return sizes
end

-- n rects stacked along one axis of area
local function stack(area, n, gap, vertical)
  local out, pos = {}, vertical and area.y or area.x
  for i, s in ipairs(parts(vertical and area.h or area.w, n, gap)) do
    if vertical then
      out[i] = { x = area.x, y = pos, w = area.w, h = s }
    else
      out[i] = { x = pos, y = area.y, w = s, h = area.h }
    end
    pos = pos + s + gap
  end
  return out
end

-- cut area in two along one axis; ratio goes to the first part
local function cut(area, ratio, gap, vertical)
  if vertical then
    local h1 = math.floor((area.h - gap) * ratio)
    return { x = area.x, y = area.y, w = area.w, h = h1 },
           { x = area.x, y = area.y + h1 + gap, w = area.w, h = area.h - h1 - gap }
  end
  local w1 = math.floor((area.w - gap) * ratio)
  return { x = area.x, y = area.y, w = w1, h = area.h },
         { x = area.x + w1 + gap, y = area.y, w = area.w - w1 - gap, h = area.h }
end

local function append(dst, src)
  for _, v in ipairs(src) do dst[#dst + 1] = v end
  return dst
end

-- Master windows on one side, the rest stacked on the other.
-- opts: ratio (0.55), nmaster (1), side ("left", "right", "top", "bottom")
function M.tile(opts)
  opts = opts or {}
  local side = opts.side or "left"
  local vertical = side == "top" or side == "bottom"
  local flip = side == "right" or side == "bottom"
  return {
    name = "tile",
    ratio = opts.ratio or 0.55,
    nmaster = opts.nmaster or 1,
    arrange = function(n, area, p)
      local nm = math.min(p.nmaster, n)
      if nm == 0 or nm == n then
        return stack(area, n, p.gap, not vertical)
      end
      local ratio = flip and 1 - p.ratio or p.ratio
      local a, b = cut(area, ratio, p.gap, vertical)
      if flip then a, b = b, a end
      return append(stack(a, nm, p.gap, not vertical), stack(b, n - nm, p.gap, not vertical))
    end,
  }
end

-- Every window takes `ratio` of what is left, splitting along the longer
-- side: a spiral of ever smaller windows.
function M.dwindle(opts)
  opts = opts or {}
  return {
    name = "dwindle",
    ratio = opts.ratio or 0.5,
    nmaster = 1,
    arrange = function(n, area, p)
      local out, rest = {}, area
      for i = 1, n do
        if i == n then
          out[i] = rest
        else
          local first, second = cut(rest, i == 1 and p.ratio or 0.5, p.gap, rest.h > rest.w)
          out[i], rest = first, second
        end
      end
      return out
    end,
  }
end

-- Rows and columns as even as possible.
function M.grid()
  return {
    name = "grid",
    ratio = 0.5,
    nmaster = 1,
    arrange = function(n, area, p)
      if n == 0 then return {} end
      local cols = math.ceil(math.sqrt(n))
      local rows = math.ceil(n / cols)
      local out = {}
      for r, row in ipairs(stack(area, rows, p.gap, true)) do
        local count = math.min(cols, n - (r - 1) * cols)
        append(out, stack(row, count, p.gap, false))
      end
      return out
    end,
  }
end

-- Every window fills the area; the focused one is on top.
function M.monocle()
  return {
    name = "monocle",
    ratio = 0.5,
    nmaster = 1,
    arrange = function(n, area)
      local out = {}
      for i = 1, n do out[i] = area end
      return out
    end,
  }
end

return M
