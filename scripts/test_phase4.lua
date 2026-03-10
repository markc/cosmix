-- test_phase4.lua — Phase 4 Script Macro Menus tests
-- Tests: script scanning, cosmix.self(), modules path, pre-addressed execution
-- Run with: cosmix run scripts/test_phase4.lua (daemon must be running)

local pass = 0
local fail = 0

local function test(name, ok, msg)
    if ok then
        pass = pass + 1
        print("  PASS: " .. name)
    else
        fail = fail + 1
        print("  FAIL: " .. name .. (msg and (" — " .. msg) or ""))
    end
end

-- ============================================
-- COSMIX.SELF() TESTS
-- ============================================
print("\n=== cosmix.self() Tests ===\n")

-- Test 1: self() returns nil when not pre-addressed
local self_port = cosmix.self()
test("cosmix.self() returns nil in CLI mode", self_port == nil)

-- ============================================
-- SCRIPT DIRECTORY TESTS
-- ============================================
print("\n=== Script Directory Tests ===\n")

-- Test 2: Script directories exist
local home = cosmix.env("HOME")
local scripts_base = home .. "/.config/cosmix/scripts"

local function dir_exists(path)
    local ok = pcall(function()
        local f = io.open(path .. "/.") -- trick to test dir
        if f then f:close() end
    end)
    -- More reliable: try to list files
    local result = cosmix.exec("test -d '" .. path .. "' && echo yes || echo no")
    return result:match("yes") ~= nil
end

test("scripts base dir exists", dir_exists(scripts_base))
test("cosmix-calc scripts dir exists", dir_exists(scripts_base .. "/cosmix-calc"))
test("cosmix-view scripts dir exists", dir_exists(scripts_base .. "/cosmix-view"))
test("cosmix-mail scripts dir exists", dir_exists(scripts_base .. "/cosmix-mail"))

-- Test 3: Script files exist
local calc_scripts = cosmix.glob(scripts_base .. "/cosmix-calc/*.lua")
test("cosmix-calc has scripts", #calc_scripts >= 2, "count=" .. #calc_scripts)

local view_scripts = cosmix.glob(scripts_base .. "/cosmix-view/*.lua")
test("cosmix-view has scripts", #view_scripts >= 1, "count=" .. #view_scripts)

local mail_scripts = cosmix.glob(scripts_base .. "/cosmix-mail/*.lua")
test("cosmix-mail has scripts", #mail_scripts >= 1, "count=" .. #mail_scripts)

-- ============================================
-- MODULES PATH TESTS
-- ============================================
print("\n=== Modules Path Tests ===\n")

-- Test 4: modules directory is in package.path
local pkg_path = package.path
test("modules dir in package.path",
    pkg_path:find("cosmix/modules") ~= nil,
    "package.path does not contain cosmix/modules")

-- ============================================
-- CLIP LIST INTEGRATION (Phase 3 + 4)
-- ============================================
print("\n=== Integration Tests ===\n")

-- Test 5: Scripts can use clip list (cross-phase integration)
cosmix.setclip("SCRIPT_TEST", "from_test_script")
local v = cosmix.getclip("SCRIPT_TEST")
test("scripts can use clip list", v == "from_test_script")
cosmix.delclip("SCRIPT_TEST")

-- Test 6: Scripts can use queues
local q = cosmix.queue("script_test_q")
q:push("item_from_script")
local item = q:pop()
test("scripts can use queues", item == "item_from_script")

-- ============================================
-- SUMMARY
-- ============================================
print("\n=== Results ===\n")
print(string.format("  %d passed, %d failed, %d total", pass, fail, pass + fail))
if fail > 0 then
    print("\n  *** SOME TESTS FAILED ***")
    os.exit(1)
else
    print("\n  All tests passed!")
end
