---
title: cosmix
section: 1
date: 2026-03-09
description: ARexx for COSMIC — desktop automation and scripting
---

# NAME

cosmix — ARexx-inspired automation system for the COSMIC desktop

# SYNOPSIS

**cosmix** _command_ [_args_...]

**cosmix** run _script_ [_args_...]

# DESCRIPTION

Cosmix is a Rust+Lua automation platform for the COSMIC desktop environment.
It provides window management, input injection, clipboard operations, D-Bus
integration, mesh networking, and a Lua scripting runtime with a rich API.

Every application can expose a "port" with named commands. Lua scripts
orchestrate these ports without touching internal app code — the modern
equivalent of Amiga's ARexx.

# COMMANDS

## Query

- **list-windows** (**lw**) — list all toplevel windows
- **list-workspaces** (**lws**) — list all workspaces
- **clipboard** (**cb**) — print clipboard text

## Window Control

- **activate** (**a**) _query_ — focus a window
- **close** (**c**) _query_ — close a window
- **minimize** (**min**) _query_ — toggle minimize
- **maximize** (**max**) _query_ — toggle maximize
- **fullscreen** (**fs**) _query_ — toggle fullscreen
- **sticky** (**st**) _query_ — toggle sticky (all workspaces)

## Input Injection

- **type** (**t**) _text_ [_delay_us_] — type text via virtual keyboard
- **key** (**k**) _combo_ [_delay_us_] — send key combo (e.g. ctrl+v, enter)

## Notifications

- **notify** (**n**) _summary_ [_body_] — send desktop notification

## Dialogs

- **dialog message** _title_ [_body_] — show message dialog
- **dialog input** _prompt_ — text input, result on stdout
- **dialog confirm** _question_ — yes/no, exit code 0/1
- **dialog list** _title_ _items_... — selection, result on stdout

## COSMIC Config

- **config-list** (**cl**) — list config components
- **config-keys** (**ck**) _component_ — list keys for component
- **config-read** (**cr**) _component_ _key_ — read config value
- **config-write** (**cw**) _component_ _key_ _value_ — write config value

## D-Bus

- **dbus** _service_ _path_ _iface_ _method_ [_args_json_] — session bus call
- **dbus-system** _service_ _path_ _iface_ _method_ [_args_json_] — system bus call
- **dbus-list** _service_ [_path_] — introspect D-Bus service

## Port Commands (ARexx-style)

- **call** _port_ _command_ [_json_] — call a command on an app port
- **list-ports** (**lp**) — list registered ports

## Clip List

- **clip set** _key_ _value_ [_ttl_secs_] — set a named value
- **clip get** _key_ — get a named value
- **clip del** _key_ — delete a named value
- **clip list** — list all clips

## Named Queues

- **queue push** _name_ _value_ — push onto queue
- **queue pop** _name_ — pop from queue
- **queue size** _name_ — get queue size
- **queue list** — list all queues

## Mesh Networking

- **mesh status** — show mesh status
- **mesh peers** — list connected peers
- **mesh send** _node_ _command_ [_json_] — send to remote node
- **mesh call** _node_ _port_ _command_ [_json_] — call remote port

## Screenshot

- **screenshot** (**ss**) [_path_] — capture full-screen PNG

## Lua Scripting

- **run** (**r**) _script_ [_args_...] — execute a Lua script
- **shell** (**sh**) — interactive Lua REPL

## Daemon

- **daemon** — start the persistent daemon
- **status** — show daemon status
- **ping** — check if daemon is running

# LUA API

Scripts have access to the **cosmix** global table:

- `cosmix.windows()`, `cosmix.workspaces()` — query desktop state
- `cosmix.activate(q)`, `cosmix.close(q)` — window control
- `cosmix.type_text(s)`, `cosmix.send_key(combo)` — input injection
- `cosmix.clipboard()`, `cosmix.set_clipboard(s)` — clipboard
- `cosmix.exec(cmd)`, `cosmix.spawn(cmd)` — process control
- `cosmix.http.get(url, opts)` — HTTP client (also post, put, patch, delete)
- `cosmix.db.open(path)` — SQLite database
- `cosmix.fmt.table(headers, rows)` — formatted table output
- `cosmix.json_encode(t)`, `cosmix.json_decode(s)` — JSON
- `cosmix.env(name)` — environment variables
- `cosmix.read_file(p)`, `cosmix.write_file(p, s)` — file I/O
- `cosmix.notify(summary, body)` — desktop notifications
- `cosmix.mail.*` — JMAP email client
- `cosmix.mesh.*` — mesh networking
- `cosmix.midi.*` — PipeWire MIDI

# FILES

- **_bin/** — executable Lua scripts (shebang: `#!/usr/bin/env -S cosmix run`)
- **_lib/cosmix/** — Lua modules (loaded via `require("cosmix.xxx")`)
- **_etc/** — configuration files
- **~/.config/cosmix/** — user configuration and scripts

# SEE ALSO

nsctl(1)
