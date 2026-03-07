-- cosmix.dialog — native iced dialog wrappers
-- Dialogs run as subprocess (cosmix dialog ...) and return results via stdout/exit code
-- Note: This is a Lua module for the cosmix desktop automation tool, not a Node.js module.
-- The cosmix.exec() calls here invoke the cosmix binary (a trusted internal tool),
-- not arbitrary user input. All arguments are shell-quoted via _quote().
local M = {}

--- Show a message dialog
function M.message(title, body)
    title = title or "Cosmix"
    body = body or ""
    cosmix.exec("cosmix dialog message " .. M._quote(title) .. " " .. M._quote(body) .. " 2>/dev/null")
end

--- Show a text input dialog, returns string or nil if cancelled
function M.input(prompt)
    prompt = prompt or "Enter value:"
    local result = cosmix.exec("cosmix dialog input " .. M._quote(prompt) .. " 2>/dev/null")
    if result and #result > 0 then
        return result
    end
    return nil
end

--- Show a yes/no confirmation dialog, returns true/false
function M.confirm(question)
    question = question or "Are you sure?"
    local result = cosmix.exec("cosmix dialog confirm " .. M._quote(question) .. " 2>/dev/null && echo yes || echo no")
    return result and result:match("yes") ~= nil
end

--- Show a list selection dialog, returns selected item or nil
function M.list(title, items)
    title = title or "Select:"
    local args = "cosmix dialog list " .. M._quote(title)
    for _, item in ipairs(items) do
        args = args .. " " .. M._quote(item)
    end
    local result = cosmix.exec(args .. " 2>/dev/null")
    if result and #result > 0 then
        return result
    end
    return nil
end

--- Shell-quote a string (prevents injection)
function M._quote(s)
    return "'" .. s:gsub("'", "'\\''") .. "'"
end

return M
