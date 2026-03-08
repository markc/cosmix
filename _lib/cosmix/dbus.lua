-- cosmix.dbus — D-Bus convenience wrappers
local M = {}

--- Control MPRIS media players
function M.playerctl(action)
    return cosmix.dbus(
        "org.mpris.MediaPlayer2.playerctld",
        "/org/mpris/MediaPlayer2",
        "org.mpris.MediaPlayer2.Player",
        action
    )
end

--- Power management via login1
function M.power(action)
    local actions = {
        suspend = "Suspend",
        hibernate = "Hibernate",
        poweroff = "PowerOff",
        reboot = "Reboot",
    }
    local method = actions[action]
    if not method then
        error("Unknown power action: " .. tostring(action) .. ". Use: suspend, hibernate, poweroff, reboot")
    end
    return cosmix.dbus_system(
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
        method,
        { true }
    )
end

--- Get NetworkManager status
function M.nm_status()
    local state = cosmix.dbus_system(
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.DBus.Properties",
        "Get",
        { "org.freedesktop.NetworkManager", "State" }
    )
    local states = {
        [0] = "unknown", [10] = "asleep", [20] = "disconnected",
        [30] = "disconnecting", [40] = "connecting",
        [50] = "connected-local", [60] = "connected-site",
        [70] = "connected-global",
    }
    local code = tonumber(state) or 0
    return { state = code, name = states[code] or "unknown" }
end

--- Get notification server info
function M.notification_server()
    return cosmix.dbus(
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "GetServerInformation"
    )
end

return M
