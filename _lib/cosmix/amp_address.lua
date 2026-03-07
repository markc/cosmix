-- cosmix.amp_address — AMP address parser (ported from nodemesh crates/amp/)
--
-- Addresses: port.app.node.amp
--   "cachyos.amp"                    → node only
--   "markweb.mko.amp"               → app + node
--   "clipboard.appmesh.cachyos.amp" → port + app + node

local M = {}
M.__index = M

--- Parse an AMP address string. Returns nil on invalid input.
function M.parse(s)
    if type(s) ~= "string" then return nil end

    -- Must end with ".amp"
    local base = s:match("^(.+)%.amp$")
    if not base then return nil end

    -- Split on dots (max 3 parts)
    local parts = {}
    for part in base:gmatch("[^%.]+") do
        parts[#parts + 1] = part
        if #parts > 3 then return nil end
    end

    local addr = setmetatable({}, M)

    if #parts == 1 then
        addr.port = nil
        addr.app = nil
        addr.node = parts[1]
    elseif #parts == 2 then
        addr.port = nil
        addr.app = parts[1]
        addr.node = parts[2]
    elseif #parts == 3 then
        addr.port = parts[1]
        addr.app = parts[2]
        addr.node = parts[3]
    else
        return nil
    end

    return addr
end

--- Check if this address targets a specific node.
function M:is_for_node(node_name)
    return self.node == node_name
end

function M:__tostring()
    local s = ""
    if self.port then s = s .. self.port .. "." end
    if self.app then s = s .. self.app .. "." end
    return s .. self.node .. ".amp"
end

return M
