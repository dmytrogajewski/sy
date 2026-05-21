-- ~/.config/yazi/init.lua
-- Initialise installed plugins. Each setup is guarded so a missing/broken
-- plugin won't take the whole UI down.

local function try_setup(name, opts)
  local ok, mod = pcall(require, name)
  if not ok then
    return
  end
  if type(mod) == "table" and type(mod.setup) == "function" then
    pcall(mod.setup, mod, opts)
  end
end

try_setup("full-border")
try_setup("dual-pane")
try_setup("autosession")
try_setup("projects")
