-- module_demo.lua — Phase 6 demo: using Lua modules
--
-- Demonstrates require() of cosmix modules from ~/.config/cosmix/modules/
--
-- Usage: cosmix run scripts/examples/module_demo.lua

-- List available modules
print("Available modules:")
for _, name in ipairs(cosmix.modules()) do
    print("  " .. name)
end
print()

-- Use textutils module
local textutils = require("textutils")

local sample = "  Hello, World!  "
print("trim: '" .. textutils.trim(sample) .. "'")

local csv = "name,age,city\nAlice,30,Brisbane\nBob,25,Gold Coast"
print("\nCSV parsing:")
local rows = textutils.parse_csv(csv)
for i, row in ipairs(rows) do
    print("  Row " .. i .. ": " .. textutils.join(row, " | "))
end

local words = "The quick brown fox jumps over the lazy dog and keeps on running through the field"
print("\nWord wrap (30 chars):")
print(textutils.wrap(words, 30))

print("\nString checks:")
print("  starts_with('cosmix-view', 'cosmix'): " .. tostring(textutils.starts_with("cosmix-view", "cosmix")))
print("  ends_with('photo.jpg', '.jpg'): " .. tostring(textutils.ends_with("photo.jpg", ".jpg")))

-- Use imagetools module (just show it loads — actual use requires cosmix-view running)
local ok, imagetools = pcall(require, "imagetools")
if ok then
    print("\nimagetools module loaded successfully")
    -- Collect images (non-destructive, just listing)
    local jpgs = imagetools.collect("/tmp/*.jpg")
    print("  Found " .. #jpgs .. " JPGs in /tmp/")
else
    print("\nimagetools module not found: " .. tostring(imagetools))
end

print("\nModule demo complete.")
