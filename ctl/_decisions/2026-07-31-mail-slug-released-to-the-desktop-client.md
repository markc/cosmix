# The `mail` slug is released to the desktop client

**Date:** 2026-07-31
**Status:** Accepted (Mark's call, twice stated)
**Scope:** `$COSMIX/src/desktop/APPS.md` identity registry; the CosMix Mail app.

## Decision

The CosMix Desktop mail reader/composer is slugged **`mail`** — source
`apps/mail`, package and binary `cosmix-mail`, app id `dev.cosmix.mail`, state
`cosmix/apps/mail`. `cosmix-maild` keeps its name and stays the backend mail
server. The APPS.md rule banning `<stem>`/`<stem>d` pairs now carries one
standing, closed exception for this pair.

## Why the rule said otherwise, and why it is being reversed

The ban was real law, not an accident: APPS.md required an app slug to differ
from every daemon *stem*, and `_doc/2026-07-20-fde-naming-registry.md` had
anticipated this exact moment, calling `cosmix-mail`/`cosmix-maild` "the
pre-scheme legacy counter-example, tolerated only until the mail client is
rebuilt as a CosMix Desktop app", with `inbox` floated as the replacement.

It is reversed on two grounds:

- **The division the ban was protecting already exists here.** The rule's
  purpose is that a daemon names a *service domain*, never its flagship GUI
  client (`studiod` declined on those grounds). `maild` passes that test on its
  own merits — it is a mail *server*, a domain with consumers beyond any client
  — so `mail` is not "the daemon's app-name spelled shorter". The two names
  denote genuinely different things.
- **The split was reserved for this on 2026-05-01**, before the registry
  existed: "maild is anything related to the daemon service, mail is reserved
  for the frontend gui client." The `mail.*` and `maild.*` ABP namespaces were
  designed to coexist on one hub without action-name collisions. The 2026-07-20
  rule generalised over that reservation without noticing it was overriding a
  deliberate one.

The correction to the premise, recorded because it matters for anyone reading
the history: APPS.md never held a `mail` row for the archived disp-skia client.
That client was carved out to `_attic/amp-display/` on 2026-07-20 and was never
registered. Nothing was un-retired here — **the never-reuse rule is intact**,
and this is the slug's first registration.

## Consequences

- APPS.md gains a `reserved` `mail` row, flipping to `active` when `apps/mail`
  lands, plus a named exception paragraph that explicitly licenses no others.
- A new daemon still bars its stem. This grandfathers one pre-existing pair.
- **Operational:** a stale `/opt/cosmix/bin/cosmix-mail` from the archived
  disp-skia app is still installed on alpha. It must be removed before the new
  binary deploys, so nobody debugs the wrong `cosmix-mail`. No deploy script
  globs `cosmix-mail*`, so nothing else confuses the pair today — keep it that
  way; the two binaries are one letter apart in a flat `/opt/cosmix/bin`.
- A future mail/calendar/contacts suite under this slug would be named for its
  first surface only. Accepted as the cost of the obvious word.
- Reconciled: `_doc/2026-07-20-fde-naming-registry.md` (bullet rewritten),
  `_doc/2026-07-29-plasma-desktop-pim-architecture.md` §8 (candidate list
  withdrawn).

## Alternatives rejected

- **Keep the ban, slug it `inbox`/`courier`/`missive`/`post`.** Rejected by
  Mark. The display name "CosMix Mail" was available either way, so the real
  question was which word keys storage and protocol paths; the reader/composer
  is what a user means by "mail", and the 2026-05-01 reservation already said
  so.
- **Rename the daemon to free the stem cleanly.** Not considered seriously:
  `maild` is live, deployed, and correctly domain-named. Renaming working
  backend infrastructure to satisfy a naming rule inverts the cost.
