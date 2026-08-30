# CosMix Tray

CosMix Tray is the windowless Plasma StatusNotifierItem for CosMix Desktop. It
ships the canonical Lucide-derived CosMix network mark as the monochrome,
Plasma-recolourable `dev.cosmix.tray` hicolor icon. Hand-tuned 16, 22 and 24
pixel status variants keep the linked-node strokes crisp in a panel; the
scalable application icon supplies the launcher. Opening the menu starts one
bounded refresh in `cosmix-trayd` while immediately rendering its last
completed snapshot; there are no health or service polling loops.

The skin performs no discovery or process control. It subscribes to the session
daemon's `Changed` signal, watches its bus-name owner across death and restart,
and renders state from one atomic `GetSnapshot` call. Menu actions send only
application slugs, manager-qualified service identities and fixed control verbs
over `dev.cosmix.trayd`; the daemon owns every argv and command line. System and
user units are labelled distinctly, partial manager failures remain visible
without hiding successful discovery, and stalled-refresh warnings are rendered
explicitly. Action failures are also reported through `notify-send` where
available.

Its stable component slug is `tray`, its package and binary are `cosmix-tray`,
and its StatusNotifierItem id is `dev.cosmix.tray`. This crate is deliberately
independent of CTK and Bevy.

```sh
cd ~/.cos/desktop
cargo run -p cosmix-tray
```
