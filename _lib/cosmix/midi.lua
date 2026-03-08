-- cosmix.midi — MIDI convenience wrappers
local M = {}

--- Find a MIDI port by pattern (case-insensitive substring match)
function M.find_port(pattern, direction)
    local ports = cosmix.midi.list_ports()
    pattern = pattern:lower()

    if direction == "input" or direction == nil then
        for _, name in ipairs(ports.inputs) do
            if name:lower():find(pattern, 1, true) then
                return name, "input"
            end
        end
    end
    if direction == "output" or direction == nil then
        for _, name in ipairs(ports.outputs) do
            if name:lower():find(pattern, 1, true) then
                return name, "output"
            end
        end
    end
    return nil
end

--- Connect output to input by name patterns
function M.connect_by_name(out_pattern, in_pattern)
    local out_name = M.find_port(out_pattern, "output")
    if not out_name then
        error("No output port matching: " .. out_pattern)
    end
    local in_name = M.find_port(in_pattern, "input")
    if not in_name then
        error("No input port matching: " .. in_pattern)
    end
    return cosmix.midi.connect(out_name, in_name)
end

--- Connect all outputs matching out_pattern to all inputs matching in_pattern
function M.route_all(out_pattern, in_pattern)
    local ports = cosmix.midi.list_ports()
    out_pattern = out_pattern:lower()
    in_pattern = in_pattern:lower()
    local count = 0
    for _, out in ipairs(ports.outputs) do
        if out:lower():find(out_pattern, 1, true) then
            for _, inp in ipairs(ports.inputs) do
                if inp:lower():find(in_pattern, 1, true) then
                    cosmix.midi.connect(out, inp)
                    count = count + 1
                end
            end
        end
    end
    return count
end

return M
