# Settings two-tier model (webd CMS/PIM shared vhost tree)

**Status:** SHIPPED 2026-06-20. Graduated from `_plan/2026-06-20-settings-amalgamation.md`
(design APPROVED by Mark). Scope: the shared webd CMS+PIM vhost tree
(`_etc/cosmix/vhosts/shared/`). No webd Rust change — Mix handlers + the cms.db
`settings` table only.

## The contract

Configuration for a vhost resolves through **two explicit tiers**:

- **Tier 1 — declarative, versioned (operator): `site.conf.mix $SITE`.** The shipped
  baseline, in git. Owns `fqdn`, `app_root`, `title`, `tagline`, `footer`, plus:
  - `admin_emails: ["…"]` — the CMS admins for this vhost (email → `admin` role).
  - `features: { blog, notes, pim_mail, pim_contacts, pim_calendar }` — per-surface
    flags (a key omitted ⇒ ON; `false` ⇒ OFF). A vhost with no `features` block runs
    every surface ON (backward-compatible).
- **Tier 2 — runtime override, mutable (admin): cms.db `settings`.** Seeded from
  `$SITE` on first boot (`cms_seed` → `set_default`, idempotent/additive), edited at
  `/admin/settings`. **db-wins.**

**Resolution (one-line answer to "where did this come from?"):**
`setting(key, $SITE-default)` returns the db value if a row exists, else the
site.conf default. `feature(key)` is that contract for a flag
(`setting("feature_"+key, feature_default_str(key)) == "1"`). So: *the site.conf
default unless an admin overrode it in the db.*

## Feature flags → surfaces

| Flag | Governs |
|---|---|
| `blog` | home feed `/`, `/post/*`, the Posts nav panels (`cms_posts` kind=`post`) |
| `notes` | `/page/*` + the Pages nav entries (kind=`page`) |
| `pim_mail` | `/pim/mail` |
| `pim_contacts` | `/pim/contacts` (+ the profile-menu Contacts link) |
| `pim_calendar` | `/pim/calendar` |

Stored as `settings` rows `feature_<key>` = `"1"`/`"0"`. **Off ⇒ hidden from the nav
AND the route answers 404** (`feature_off_doc`) — a feature is genuinely *not present*
on that vhost, not merely unlinked. `/pim` (bare) redirects to the first enabled PIM
surface, or `/` if all are off. **Admin surfaces (`/admin/*`) are NOT gated** — staff
keep managing content while a public surface is off, so it can be prepared then flipped on.

## Where it lives (code)

- `lib.mix` — `feature_keys()`, `feature_default_str()`, `feature()`, `seed_admins()`,
  `feature_off_doc()`; `cms_seed()` seeds `feature_*` defaults + calls `seed_admins()`;
  `build_nav_pages`/`build_nav_posts`/`page()` consult `feature()`.
- `h_home/h_post/h_page` + `h_pim_*` — per-route gate.
- `h_admin_settings.mix` — the ONE managed surface: identity + a Features section
  (one `fm_toggle` per flag) + **Reset to site.conf defaults** (re-asserts Tier 1).
- `site.conf.mix` (example.com / example.net / alpha.example.org) — `admin_emails` + `features`.

## Admin seeding semantics

`seed_admins()` is idempotent and runs every request (via `cms_init`→`cms_seed`):
**ensure-admin** — insert a missing `admin_emails` address as `admin`, promote an
existing non-admin row; writes only on an actual change (no WAL churn). Emails are
**lowercased** before seeding (maild presents `$SESSION["email"]` — the
`current_user()` username lookup — as the canonical lowercase address, so a mixed-case
`admin_emails` typo can't seed a row the session never matches); keep `admin_emails`
lowercase. A listed admin demoted via `/admin/users` is **re-promoted on the next
request** — to change admins, edit `site.conf.mix`. `seed_admin.mix` (the CLI) remains for ad-hoc/manual promotion;
the declarative path supersedes it for the three managed vhosts. No cms.db *password*
admin is ever seeded (the unified maild login authenticates; `pass_hash`/`salt` are
NOT-NULL filler, never usable).
