-- cosmix.email — email text extractor (ported from nodemesh crates/docparse/)
--
-- Parses .eml files and extracts plain text content with metadata.
-- For simple header + body extraction without external dependencies.

local M = {}

--- Strip HTML tags from a string.
function M.strip_html(html)
    if not html then return "" end
    local result = {}
    local in_tag = false
    for i = 1, #html do
        local ch = html:sub(i, i)
        if ch == "<" then
            in_tag = true
        elseif ch == ">" then
            in_tag = false
        elseif not in_tag then
            result[#result + 1] = ch
        end
    end
    return table.concat(result)
end

--- Parse a raw .eml file string into structured data.
-- Returns { subject, from, date, to, body, headers, metadata }
function M.parse(raw)
    if type(raw) ~= "string" then return nil, "expected string" end

    -- Split headers from body at first blank line
    local header_block, body
    local sep = raw:find("\r?\n\r?\n")
    if sep then
        header_block = raw:sub(1, sep - 1)
        body = raw:sub(raw:find("\n", sep + 1) or sep + 2)
        -- Skip past the blank line(s)
        body = body:gsub("^[\r\n]+", "")
    else
        header_block = raw
        body = ""
    end

    -- Parse headers (handles continuation lines)
    local headers = {}
    local last_key
    for line in header_block:gmatch("[^\r\n]+") do
        if line:match("^%s") and last_key then
            -- Continuation of previous header
            headers[last_key] = headers[last_key] .. " " .. line:match("^%s+(.*)$")
        else
            local k, v = line:match("^([%w%-]+):%s*(.*)$")
            if k then
                k = k:lower()
                headers[k] = v
                last_key = k
            end
        end
    end

    -- Extract key fields
    local subject = headers["subject"]
    local from = headers["from"]
    local date = headers["date"]
    local to = headers["to"]
    local content_type = headers["content-type"] or ""

    -- Build readable text output
    local parts = {}
    if subject then parts[#parts + 1] = "Subject: " .. subject end
    if from then parts[#parts + 1] = "From: " .. from end
    if date then parts[#parts + 1] = "Date: " .. date end
    if #parts > 0 then parts[#parts + 1] = "" end

    -- If HTML content, strip tags
    if content_type:lower():find("text/html") then
        parts[#parts + 1] = M.strip_html(body)
    else
        parts[#parts + 1] = body
    end

    local text = table.concat(parts, "\n")

    return {
        subject = subject,
        from = from,
        date = date,
        to = to,
        body = body,
        text = text,
        headers = headers,
        metadata = {
            mime_type = "message/rfc822",
            title = subject,
            author = from,
            date = date,
        },
    }
end

--- Parse an .eml file from disk.
function M.parse_file(path)
    local f, err = io.open(path, "r")
    if not f then return nil, "failed to read file: " .. (err or path) end
    local raw = f:read("*a")
    f:close()
    return M.parse(raw)
end

return M
