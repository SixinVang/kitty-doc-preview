if vim.g.loaded_docp then
  return
end
vim.g.loaded_docp = true

require("docp").bootstrap()
