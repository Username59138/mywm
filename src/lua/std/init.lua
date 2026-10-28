-- mywm.std: the standard library, all in one table.
--
--   local std = require("mywm.std")
--   local desk = std.desktop.new { ... }
--   local binds = std.binds.new()

return {
  desktop = require("mywm.desktop"),
  layouts = require("mywm.layouts"),
  binds   = require("mywm.binds"),
}
