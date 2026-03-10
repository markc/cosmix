-- watcher.lua — Phase 5 demo: event-driven watcher pattern
--
-- Long-running script that monitors the system and reacts to events.
-- Demonstrates cosmix.on() for event handling and cosmix.every() for polling.
--
-- Usage: cosmix run scripts/examples/watcher.lua

print("Starting watcher — monitoring ports and clipboard...")
print("Press Ctrl+C to stop")

-- Track known ports
local known_ports = {}
for _, p in ipairs(cosmix.list_ports()) do
    known_ports[p.name] = true
    print("  Found port: " .. p.name)
end

-- Poll for port changes every 3 seconds
local check_count = 0
while true do
    check_count = check_count + 1

    -- Check for new/removed ports
    local current = {}
    for _, p in ipairs(cosmix.list_ports()) do
        current[p.name] = true
        if not known_ports[p.name] then
            print("[" .. os.date("%H:%M:%S") .. "] New port: " .. p.name)
            cosmix.notify("Port Appeared", p.name .. " is now available")

            -- Auto-query new app ports
            if p.type == "app" then
                local port = cosmix.port(p.name)
                local info = port:send("help")
                if info.ok and info.data then
                    print("  Commands: " .. cosmix.json_encode(info.data))
                end
            end
        end
    end

    for name, _ in pairs(known_ports) do
        if not current[name] then
            print("[" .. os.date("%H:%M:%S") .. "] Port removed: " .. name)
        end
    end
    known_ports = current

    -- Periodic status (every 10 checks = 30s)
    if check_count % 10 == 0 then
        local port_count = 0
        for _ in pairs(known_ports) do port_count = port_count + 1 end
        print("[" .. os.date("%H:%M:%S") .. "] Status: " .. port_count .. " ports active")
    end

    cosmix.sleep(3000)
end
