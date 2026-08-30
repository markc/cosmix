# cosmix-lib-davproto

`cosmix-lib-davproto` provides shared Rust codecs and representation helpers for
CalDAV and CardDAV data. It belongs to the `cos` library layer in the
`bus <- mix <- cos` dependency chain and has no direct dependency on a Bus or
Mix crate.

## Synopsis

The crate converts between structured calendar or contact data and the text
formats used by DAV clients. It also derives strong content ETags for
conditional requests.

The public API covers:

- JSCalendar event fields to iCalendar `VCALENDAR` and `VEVENT` text.
- iCalendar parsing into indexed event fields and stored JSON data.
- In-place patching of selected iCalendar event properties.
- JSContact fields to vCard 4.0 text.
- vCard parsing into indexed contact fields and stored JSON data.
- In-place patching of selected vCard properties.
- Quoted BLAKE3 ETags over bytes, events, and contacts.

This is a library crate. It has no executable, command-line interface,
configuration format, subcommands, or Bus verbs.

## Modules

| Module | Purpose |
| --- | --- |
| `etag` | Builds strong, deterministic content ETags. |
| `ical` | Emits, parses, and patches iCalendar event representations. |
| `vcard` | Emits, parses, and patches vCard contact representations. |

## iCalendar

The `ical` module handles a single `VEVENT` inside a `VCALENDAR`.

### Emission

`event_to_ics` builds iCalendar text from:

- `uid` as `UID`.
- `title` as `SUMMARY`.
- UTC `start` and `end` values as `DTSTART` and `DTEND`.
- `updated` as `LAST-MODIFIED`.
- JSON `description` as `DESCRIPTION`.
- The first JSON `locations` entry with a `name` as `LOCATION`.

The emitted event always receives a deterministic `DTSTAMP`. The function uses
`updated`, then `start`, then the Unix epoch. It does not use the current time,
so repeated reads of unchanged inputs produce the same representation.

`RAW_ICAL_KEY` is `cosmix:rawICalendar`. When the JSON data contains a string
under this key, `event_to_ics` returns that string unchanged instead of
rebuilding the event. This preserves fields that the structured mapper does not
model.

### Parsing

`parse_ics` parses iCalendar text and returns `ParsedEvent`. The result exposes:

| Field | Meaning |
| --- | --- |
| `uid` | The first event's optional `UID`. |
| `title` | The first event's optional `SUMMARY`. |
| `start` | The first event's optional indexed start time in UTC. |
| `end` | The first event's optional indexed end time in UTC. |
| `data` | A JSCalendar-shaped JSON object containing mapped fields and the raw input. |

The parser selects the first `VEVENT` and returns an error when none exists.
UTC date-times remain UTC. Floating and timezone-qualified local date-times are
treated as UTC for indexing. Date-only values become midnight UTC.

The original input is stored under `RAW_ICAL_KEY`. A later emit therefore
round-trips the complete client representation, including recurrence rules,
alarms, timezone components, and extension properties.

### Patching

`patch_ics` updates `SUMMARY`, `DTSTART`, and `DTEND` in an existing iCalendar
body. It unfolds continued lines before matching properties and changes only
properties inside `VEVENT`.

Requested properties that are absent are inserted before `END:VEVENT`.
Untouched lines remain present. Updated date-times use basic UTC form:
`YYYYMMDDTHHMMSSZ`.

Passing `None` for a property leaves it unchanged. Rewritten start and end
properties lose existing `TZID` and `VALUE` parameters.

## vCard

The `vcard` module handles vCard 4.0 contact text.

### Emission

`contact_to_vcf` maps:

- `uid` to `UID`.
- `full_name` to the required `FN`.
- `email` to `EMAIL`.
- `company` to `ORG`.
- The primary phone in JSON data to `TEL`.

When `full_name` is absent, `FN` falls back to JSContact `name.full`, then to
the UID. Output lines use CRLF termination.

`RAW_VCARD_KEY` is `cosmix:rawVCard`. When the JSON data contains a string under
this key, `contact_to_vcf` returns it unchanged.

### Parsing

`parse_vcf` unfolds continued lines and reads the first `UID`, `FN`, `EMAIL`,
`ORG`, and `TEL`. It returns `ParsedContact` with:

| Field | Meaning |
| --- | --- |
| `uid` | The optional `UID`. |
| `full_name` | The required formatted name from `FN`. |
| `email` | The optional first email address. |
| `company` | The first component of the optional `ORG` value. |
| `data` | A JSContact-shaped JSON object containing mapped fields and the raw input. |

The parser returns an error when `FN` is absent. A parsed phone is placed at
`phones.default.number`. The complete original vCard is stored under
`RAW_VCARD_KEY`, preserving unmodelled properties for later reads.

### Patching and phone helpers

`patch_vcf` updates `FN`, `EMAIL`, `ORG`, and `TEL`. It preserves each matched
property's parameter prefix, such as `EMAIL;TYPE=work`, and retains all other
lines. A requested property that is absent is inserted before the closing
`END` line.

Only non-empty replacement values are applied. `None` and empty strings leave
the corresponding property unchanged.

`primary_phone` reads the first phone number from the JSContact `phones` map.
If that map has no usable number, it reads the first `TEL` value from a stashed
raw vCard.

`set_primary_phone` updates the first existing `phones` entry while preserving
its sibling fields and other phone entries. If no entry exists, it creates
`phones.default.number`.

## ETags

`etag::strong` returns a quoted hexadecimal BLAKE3 digest:

```text
"<blake3-hex>"
```

`etag::for_event` hashes the event UID, title, start, end, updated value, and
JSON data as one deterministic JSON value.

`etag::for_contact` hashes the contact UID, full name, email, company, and JSON
data in the same way.

These helpers hash source fields rather than emitted iCalendar or vCard text.
An emitted-field change changes the ETag, while representation formatting
changes alone do not.

## Scope and limitations

The structured iCalendar emitter does not model recurrence, alarms,
participants, or `VTIMEZONE`. The raw iCalendar path preserves them.

The structured vCard emitter handles one formatted name, email, organisation,
and primary phone. It does not emit structured `N` values or multiple values
for these fields. The raw vCard path preserves richer client data.

The crate declares no Cargo features.

## Dependencies

| Dependency | Use |
| --- | --- |
| `serde_json` | Structured JSCalendar and JSContact values and deterministic ETag inputs. |
| `chrono` | UTC calendar date-times. |
| `anyhow` | Parse errors returned by the public parsing functions. |
| `icalendar` | iCalendar parsing and event emission. |
| `blake3` | Strong content hashing. |
