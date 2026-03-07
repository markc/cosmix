-- cosmix.mesh — mesh routing and metrics (ported from nodemesh crates/meshd/)
--
-- Route table management, metrics counters, and config loading for mesh nodes.
-- The async networking stays in Rust; this handles the data-driven logic.

local amp = require("cosmix.amp")

local M = {}

---------------------------------------------------------------------------
-- Metrics — simple counters for observability
---------------------------------------------------------------------------

local Metrics = {}
Metrics.__index = Metrics

function Metrics.new()
    return setmetatable({
        sent = 0,
        received = 0,
        dropped = 0,
    }, Metrics)
end

function Metrics:inc_sent() self.sent = self.sent + 1 end
function Metrics:inc_received() self.received = self.received + 1 end
function Metrics:inc_dropped() self.dropped = self.dropped + 1 end

function Metrics:snapshot()
    return {
        sent = self.sent,
        received = self.received,
        dropped = self.dropped,
    }
end

function Metrics:reset()
    self.sent = 0
    self.received = 0
    self.dropped = 0
end

M.Metrics = Metrics

---------------------------------------------------------------------------
-- Route table — maps node names to peer info
---------------------------------------------------------------------------

local RouteTable = {}
RouteTable.__index = RouteTable

function RouteTable.new(node_name)
    return setmetatable({
        node_name = node_name,
        peers = {},   -- name -> { wg_ip, port, connected }
    }, RouteTable)
end

--- Add or update a peer in the route table.
function RouteTable:set_peer(name, wg_ip, port)
    self.peers[name] = {
        wg_ip = wg_ip,
        port = port or 9800,
        connected = false,
    }
end

--- Remove a peer from the route table.
function RouteTable:remove_peer(name)
    self.peers[name] = nil
end

--- Mark a peer as connected/disconnected.
function RouteTable:set_connected(name, connected)
    if self.peers[name] then
        self.peers[name].connected = connected
    end
end

--- Resolve an AMP address to a peer name. Returns nil if local or unknown.
function RouteTable:resolve(amp_address)
    local addr = require("cosmix.amp_address")
    local parsed = addr.parse(amp_address)
    if not parsed then return nil end

    -- If it's for this node, it's local
    if parsed:is_for_node(self.node_name) then return nil end

    -- Look up the target node in peers
    local peer = self.peers[parsed.node]
    if peer then return parsed.node end

    return nil
end

--- Get list of connected peer names.
function RouteTable:connected_peers()
    local result = {}
    for name, info in pairs(self.peers) do
        if info.connected then
            result[#result + 1] = name
        end
    end
    return result
end

--- Get full status of all peers.
function RouteTable:status()
    local result = {}
    for name, info in pairs(self.peers) do
        result[#result + 1] = {
            name = name,
            wg_ip = info.wg_ip,
            port = info.port,
            connected = info.connected,
        }
    end
    table.sort(result, function(a, b) return a.name < b.name end)
    return result
end

M.RouteTable = RouteTable

---------------------------------------------------------------------------
-- Config loader — reads meshd TOML config into Lua tables
---------------------------------------------------------------------------

--- Load a meshd config file. Requires a TOML parser (toml.lua or similar).
-- Returns a config table matching meshd's structure, or nil + error.
function M.load_config(path)
    -- Try to use toml library if available
    local ok, toml = pcall(require, "toml")
    if not ok then
        -- Fall back to simple key-value parsing for flat configs
        return M._parse_simple_config(path)
    end

    local f, err = io.open(path, "r")
    if not f then return nil, "failed to read config: " .. (err or path) end
    local content = f:read("*a")
    f:close()

    local config = toml.parse(content)
    return config
end

--- Simple TOML-like config parser (handles sections and key=value).
function M._parse_simple_config(path)
    local f, err = io.open(path, "r")
    if not f then return nil, "failed to read config: " .. (err or path) end

    local config = {}
    local section = config

    for line in f:lines() do
        line = line:match("^%s*(.-)%s*$") -- trim
        if line == "" or line:sub(1, 1) == "#" then
            -- skip blank/comment
        elseif line:match("^%[([%w%-%.]+)%]$") then
            -- Section header: [node] or [peers.mko]
            local name = line:match("^%[([%w%-%.]+)%]$")
            local parts = {}
            for part in name:gmatch("[^%.]+") do
                parts[#parts + 1] = part
            end
            section = config
            for _, part in ipairs(parts) do
                section[part] = section[part] or {}
                section = section[part]
            end
        else
            -- Key = value
            local k, v = line:match('^([%w_%-]+)%s*=%s*"(.-)"$')
            if not k then
                k, v = line:match("^([%w_%-]+)%s*=%s*(.+)$")
                if v then
                    -- Parse booleans and numbers
                    if v == "true" then v = true
                    elseif v == "false" then v = false
                    else v = tonumber(v) or v end
                end
            end
            if k then section[k] = v end
        end
    end

    f:close()
    return config
end

---------------------------------------------------------------------------
-- Dispatch — route an AMP message to the right destination
---------------------------------------------------------------------------

--- Determine where a message should go based on its "to" address.
-- Returns: "local", "peer:<name>", or "drop"
function M.dispatch(route_table, msg)
    local to = msg:to_addr()
    if not to then return "local" end

    local peer = route_table:resolve(to)
    if peer then
        return "peer:" .. peer
    end

    -- Check if it's for this node
    local addr = require("cosmix.amp_address")
    local parsed = addr.parse(to)
    if parsed and parsed:is_for_node(route_table.node_name) then
        return "local"
    end

    return "drop"
end

return M
