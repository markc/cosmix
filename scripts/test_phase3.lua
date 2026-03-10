-- test_phase3.lua — Phase 3 Clip List and Queues comprehensive test
-- Run with: cosmix run scripts/test_phase3.lua (daemon must be running)

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
-- CLIP LIST TESTS
-- ============================================
print("\n=== Clip List Tests ===\n")

-- Clean slate
cosmix.delclip("TEST_STRING")
cosmix.delclip("TEST_NUMBER")
cosmix.delclip("TEST_TABLE")
cosmix.delclip("TEST_BOOL")
cosmix.delclip("TEST_TTL")
cosmix.delclip("TEST_OVERWRITE")

-- Test 1: Set and get a string
cosmix.setclip("TEST_STRING", "hello world")
local v = cosmix.getclip("TEST_STRING")
test("setclip/getclip string", v == "hello world", tostring(v))

-- Test 2: Set and get a number
cosmix.setclip("TEST_NUMBER", 42)
local v2 = cosmix.getclip("TEST_NUMBER")
test("setclip/getclip number", v2 == 42, tostring(v2))

-- Test 3: Set and get a boolean
cosmix.setclip("TEST_BOOL", true)
local v3 = cosmix.getclip("TEST_BOOL")
test("setclip/getclip boolean", v3 == true, tostring(v3))

-- Test 4: Set and get a table (JSON object)
cosmix.setclip("TEST_TABLE", {name = "cosmix", version = 1})
local v4 = cosmix.getclip("TEST_TABLE")
test("setclip/getclip table", v4 ~= nil and v4.name == "cosmix" and v4.version == 1,
    v4 and ("name=" .. tostring(v4.name)) or "nil")

-- Test 5: Get non-existent key returns nil
local v5 = cosmix.getclip("NONEXISTENT_KEY_12345")
test("getclip missing key returns nil", v5 == nil)

-- Test 6: Overwrite existing key
cosmix.setclip("TEST_OVERWRITE", "first")
cosmix.setclip("TEST_OVERWRITE", "second")
local v6 = cosmix.getclip("TEST_OVERWRITE")
test("overwrite clip", v6 == "second", tostring(v6))

-- Test 7: Delete a clip
cosmix.setclip("TEST_DELETE_ME", "gone")
local deleted = cosmix.delclip("TEST_DELETE_ME")
test("delclip returns true", deleted == true)
local v7 = cosmix.getclip("TEST_DELETE_ME")
test("deleted clip returns nil", v7 == nil)

-- Test 8: Delete non-existent key
local deleted2 = cosmix.delclip("NEVER_EXISTED_12345")
test("delclip non-existent returns false", deleted2 == false)

-- Test 9: List clips (should include our test clips)
local clips = cosmix.listclips()
test("listclips returns table", type(clips) == "table")
local found_string = false
for _, c in ipairs(clips) do
    if c.key == "TEST_STRING" then
        found_string = true
        test("listclips entry has value", c.value == "hello world")
        test("listclips entry has set_by", c.set_by == "lua")
        test("listclips entry has set_at", type(c.set_at) == "number" and c.set_at > 0)
    end
end
test("listclips contains TEST_STRING", found_string)

-- Test 10: TTL support
cosmix.setclip("TEST_TTL", "expires soon", {ttl = 3600})
local v10 = cosmix.getclip("TEST_TTL")
test("setclip with TTL", v10 == "expires soon")

-- Test 11: TTL=1 should still be valid immediately
cosmix.setclip("TEST_TTL_SHORT", "short lived", {ttl = 1})
local v11 = cosmix.getclip("TEST_TTL_SHORT")
test("TTL=1 valid immediately", v11 == "short lived")

-- Cleanup test clips
cosmix.delclip("TEST_STRING")
cosmix.delclip("TEST_NUMBER")
cosmix.delclip("TEST_TABLE")
cosmix.delclip("TEST_BOOL")
cosmix.delclip("TEST_TTL")
cosmix.delclip("TEST_TTL_SHORT")
cosmix.delclip("TEST_OVERWRITE")

-- ============================================
-- NAMED QUEUE TESTS
-- ============================================
print("\n=== Named Queue Tests ===\n")

-- Test 12: Create queue and push
local q = cosmix.queue("test_work")
q:push("item1")
q:push("item2")
q:push("item3")
test("queue push + size", q:size() == 3, "size=" .. q:size())

-- Test 13: FIFO order
local first = q:pop()
test("queue FIFO pop first", first == "item1", tostring(first))
local second = q:pop()
test("queue FIFO pop second", second == "item2", tostring(second))
test("queue size after 2 pops", q:size() == 1, "size=" .. q:size())

-- Test 14: Pop remaining
local third = q:pop()
test("queue pop last", third == "item3", tostring(third))
test("queue empty after all pops", q:size() == 0)

-- Test 15: Pop from empty queue returns nil
local empty = q:pop()
test("pop empty queue returns nil", empty == nil)

-- Test 16: Queue with structured data
local q2 = cosmix.queue("test_structured")
q2:push({file = "/tmp/a.png", action = "rotate"})
q2:push({file = "/tmp/b.png", action = "crop"})
local item = q2:pop()
test("queue structured data", item ~= nil and item.file == "/tmp/a.png" and item.action == "rotate",
    item and item.file or "nil")

-- Test 17: Multiple independent queues
local qa = cosmix.queue("test_alpha")
local qb = cosmix.queue("test_beta")
qa:push("a1")
qb:push("b1")
qb:push("b2")
test("independent queues alpha size", qa:size() == 1)
test("independent queues beta size", qb:size() == 2)
local a1 = qa:pop()
test("independent queues alpha pop", a1 == "a1")
test("independent queues beta still 2", qb:size() == 2)

-- Test 18: Clear queue
qb:clear()
test("queue clear", qb:size() == 0)
local after_clear = qb:pop()
test("pop after clear returns nil", after_clear == nil)

-- Cleanup
q2:clear()
qa:clear()

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
