if vim.g.loaded_docp then
  return
end
vim.g.loaded_docp = true

vim.api.nvim_create_user_command("DocpPreview", function()
  require("docp").open()
end, { desc = "Open a docp preview pane synced with this buffer" })

vim.api.nvim_create_user_command("DocpClose", function()
  require("docp").close()
end, { desc = "Close this buffer's docp preview pane" })

vim.api.nvim_create_user_command("DocpToggle", function()
  require("docp").toggle()
end, { desc = "Toggle this buffer's docp preview pane" })

vim.api.nvim_create_autocmd("VimLeavePre", {
  callback = function()
    require("docp").close_all()
  end,
})
