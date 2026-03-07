-- cosmix.helpers — reusable utilities for cosmix scripts
local M = {}

--- Pretty-print a table
function M.dump(t, indent)
    indent = indent or ""
    if type(t) ~= "table" then
        print(indent .. tostring(t))
        return
    end
    for k, v in pairs(t) do
        if type(v) == "table" then
            print(indent .. tostring(k) .. ":")
            M.dump(v, indent .. "  ")
        else
            print(indent .. tostring(k) .. " = " .. tostring(v))
        end
    end
end

--- Filter windows by predicate function
function M.filter_windows(fn)
    local wins = cosmix.windows()
    local result = {}
    for _, w in ipairs(wins) do
        if fn(w) then
            result[#result + 1] = w
        end
    end
    return result
end

--- Find first window matching app_id substring
function M.find_window(query)
    query = query:lower()
    for _, w in ipairs(cosmix.windows()) do
        if w.app_id:lower():find(query, 1, true) or w.title:lower():find(query, 1, true) then
            return w
        end
    end
    return nil
end

--- Get the active workspace name
function M.active_workspace()
    for _, ws in ipairs(cosmix.workspaces()) do
        if ws.active then return ws.name end
    end
    return nil
end

--- UTF-8 safe truncation (won't split multi-byte characters)
function M.truncate(s, maxlen)
    if #s <= maxlen then return s end
    local i = maxlen
    -- Walk back to find a valid UTF-8 boundary
    while i > 0 and s:byte(i) >= 0x80 and s:byte(i) < 0xC0 do
        i = i - 1
    end
    -- If we landed on a lead byte, remove it too (it would be incomplete)
    if i > 0 and s:byte(i) >= 0xC0 then
        i = i - 1
    end
    return s:sub(1, i)
end

return M
