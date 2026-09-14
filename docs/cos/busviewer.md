# BusViewer

BusViewer is a native CTK companion to the `bus` CLI, modelled on QDBusViewer.
It opens a dark 1400 × 1400 window with File/Help menus, a searchable service
tree, a draggable divider, verb details, a JSON input and a reply panel.

Build the `cosmix-busviewer` package in the separate `src/desktop` workspace.
Its binary is `busviewer`; its application id is `dev.cosmix.busviewer` and
its installation metadata component is `busviewer`.

Launch `busviewer` using the shared node configuration, or supply
`--noded-url URL` to choose a broker. `--help` and `--version` work without a
desktop session. Typography follows CTK's shared `COSMIX_UI_FONT` and
`COSMIX_UI_FONT_PX` settings, including system clipboard support.

Expand a service and select a verb to see its argument signature,
description and read-only flag. Enter an optional JSON body and press Call.
Blank input sends no body; invalid JSON sends no request. The reply includes
the target, verb, actual Bus return code and pretty-printed JSON (or verbatim
text when the reply is not JSON). Transport errors are separate from service
errors. Calls time out after 20 seconds and are never automatically retried;
a timeout can leave the remote outcome unknown. Only one call is active at
a time. Selecting another verb does not relabel an outstanding reply.

Discovery uses NodedClient over ABP: `noded.list`, `noded.peers`, and universal
`HELP`, with `app.describe` fallback for older services. Legacy descriptions
without a read-only flag display “unknown”. The broker itself is included
even when absent from its citizen registry. HELP requests run in the
background with at most eight concurrent service inspections. Search matches
service names and loaded verb names, signatures and descriptions. Use
File → Refresh to reload services and retry failed descriptions.

Some broker versions do not implement HELP or app.describe for `noded`
itself. That entry shows an introspection error; other services remain
available. BusViewer does not invent a broker verb catalogue.

The Mesh nodes branch lists authority routing members, excluding this node,
and falls back to the older peers list when authority metadata is absent.
Membership alone does not indicate a live connection. Remote service
drill-down is planned; version 0.1 calls services through the selected local
broker. Replies larger than one million characters are explicitly truncated
for display.

The app reuses CTK 0.54.4 tree views/disclosures, text fields, read-only text
areas, buttons, menus and the existing DCS split widget. It does not embed
the DCS application shell or add desktop furniture.
