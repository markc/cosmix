-- orchestrator.lua — Phase 5 demo: multi-app orchestration
--
-- The canonical ARexx power demo translated to cosmix.
-- Launches apps if not running, coordinates commands across ports.
--
-- Usage: cosmix run scripts/examples/orchestrator.lua

-- Launch cosmix-calc if not running
if not cosmix.port_exists("cosmix-calc") then
    print("Launching cosmix-calc...")
    cosmix.launch("cosmix-calc", { wait = true, timeout = 5000 })
    if not cosmix.port_exists("cosmix-calc") then
        error("Failed to start cosmix-calc")
    end
end

-- Launch cosmix-view if not running
if not cosmix.port_exists("cosmix-view") then
    print("Launching cosmix-view...")
    cosmix.launch("cosmix-view", { wait = true, timeout = 5000 })
    if not cosmix.port_exists("cosmix-view") then
        error("Failed to start cosmix-view")
    end
end

-- Use ADDRESS style: set default port, send commands without specifying port
cosmix.address("cosmix-calc")
local result = cosmix.send("calc", { expression = "1250 * 1.1" })
if result.ok then
    print("Calculation result: " .. tostring(result.data))
else
    print("Calc error: " .. (result.error or "unknown"))
end

-- Switch to view
cosmix.address("cosmix-view")
local info = cosmix.send("info")
if info.ok then
    print("View port info: " .. cosmix.json_encode(info.data))
end

-- Or use explicit port handles (works alongside ADDRESS)
local calc = cosmix.port("cosmix-calc")
local view = cosmix.port("cosmix-view")

-- Get calc result and publish to clip list
calc:send("calc", { expression = "42 * 3.14159" })
local total = calc:send("result")
if total.ok then
    cosmix.setclip("LAST_CALC_RESULT", total.data)
    print("Published result to clip list: " .. tostring(total.data))
end

-- Notify completion
cosmix.notify("Orchestrator Complete", "Multi-app orchestration finished")
print("Done.")
