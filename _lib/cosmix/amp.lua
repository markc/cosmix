-- cosmix.amp — AMP wire protocol (ported from nodemesh crates/amp/)
--
-- Wire format:
--   ---
--   key: value
--   ---
--   optional body text
--
-- The empty message "---\n---\n" is valid (heartbeat/keepalive).

local json = require("cjson") or require("dkjson") or nil

local M = {}
M.__index = M

local EMPTY_WIRE = "---\n---\n"

local KNOWN_HEADERS = {
    amp = true, type = true, id = true, from = true, to = true,
    command = true, args = true, json = true, ["reply-to"] = true,
    ttl = true, error = true, timestamp = true,
}

local VALID_TYPES = {
    request = true, response = true, event = true, stream = true,
}

--- Create a new AMP message.
function M.new(headers, body)
    return setmetatable({
        headers = headers or {},
        body = body or "",
    }, M)
end

--- Create the empty AMP message (heartbeat/keepalive).
function M.empty()
    return M.new({}, "")
end

--- Create a command message (headers only, no body).
function M.command(headers)
    return M.new(headers, "")
end

--- Parse a raw AMP wire message. Returns nil on invalid input.
function M.parse(raw)
    if type(raw) ~= "string" then return nil end
    -- Must start with "---\n"
    if raw:sub(1, 4) ~= "---\n" then return nil end

    local content = raw:sub(5)
    local frontmatter, body

    -- Find closing "---\n" delimiter
    local sep_start, sep_end = content:find("\n---\n")
    if sep_start then
        frontmatter = content:sub(1, sep_start - 1)
        body = content:sub(sep_end + 1)
    else
        -- Could be empty message: content is just "---\n"
        if content:sub(1, 4) == "---\n" or content == "---" then
            frontmatter = ""
            body = ""
        else
            return nil
        end
    end

    -- Parse headers
    local headers = {}
    for line in frontmatter:gmatch("[^\n]+") do
        local k, v = line:match("^(.-):%s+(.*)$")
        if k then
            headers[k] = v
        end
    end

    -- Trim trailing newline from body
    if body:sub(-1) == "\n" then
        body = body:sub(1, -2)
    end

    return M.new(headers, body)
end

--- Serialize to AMP wire format.
function M:to_wire()
    local parts = { "---" }
    for k, v in pairs(self.headers) do
        parts[#parts + 1] = k .. ": " .. v
    end
    parts[#parts + 1] = "---"

    local wire = table.concat(parts, "\n") .. "\n"
    if self.body ~= "" then
        wire = wire .. self.body
        if self.body:sub(-1) ~= "\n" then
            wire = wire .. "\n"
        end
    end
    return wire
end

--- Check if this is the empty message (heartbeat/keepalive).
function M:is_empty()
    return next(self.headers) == nil and self.body == ""
end

--- Get a header value.
function M:get(key)
    return self.headers[key]
end

--- Get the "from" address.
function M:from_addr()
    return self.headers["from"]
end

--- Get the "to" address.
function M:to_addr()
    return self.headers["to"]
end

--- Get the "command" header.
function M:command_name()
    return self.headers["command"]
end

--- Get the "type" header.
function M:message_type()
    return self.headers["type"]
end

--- Get "args" as parsed JSON table. Returns nil on missing/invalid.
function M:args()
    local raw = self.headers["args"]
    if not raw or not json then return nil end
    local ok, val = pcall(json.decode, raw)
    return ok and val or nil
end

--- Get "json" payload as parsed table. Returns nil on missing/invalid.
function M:json_payload()
    local raw = self.headers["json"]
    if not raw or not json then return nil end
    local ok, val = pcall(json.decode, raw)
    return ok and val or nil
end

--- Validate for protocol conformance. Returns list of warnings (empty = valid).
function M:validate()
    local warnings = {}

    if self:is_empty() then return warnings end

    for key in pairs(self.headers) do
        if not KNOWN_HEADERS[key] then
            warnings[#warnings + 1] = "unknown header: " .. key
        end
    end

    local msg_type = self.headers["type"]
    if msg_type and not VALID_TYPES[msg_type] then
        warnings[#warnings + 1] = "invalid type: " .. msg_type
    end

    if self.headers["args"] and json then
        local ok = pcall(json.decode, self.headers["args"])
        if not ok then
            warnings[#warnings + 1] = "args is not valid JSON"
        end
    end

    if self.headers["json"] and json then
        local ok = pcall(json.decode, self.headers["json"])
        if not ok then
            warnings[#warnings + 1] = "json payload is not valid JSON"
        end
    end

    local ttl = self.headers["ttl"]
    if ttl and not tonumber(ttl) then
        warnings[#warnings + 1] = "ttl is not a valid integer: " .. ttl
    end

    return warnings
end

function M:__tostring()
    return self:to_wire()
end

--- Constants
M.EMPTY_WIRE = EMPTY_WIRE
M.KNOWN_HEADERS = KNOWN_HEADERS
M.VALID_TYPES = VALID_TYPES

return M
