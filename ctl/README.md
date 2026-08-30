# `ctl/` — the starter control folder

CosMix separates **the public system** (this repository, `$COSMIX`) from
**your private control hub** (`$CMCTL`, conventionally `~/.ctl`): the specs and decisions you are working
against, your plans and journals, your deploy scripts, your machine
inventory, and the gate that keeps private values out of anything you
publish. This folder is the public **starter** for that hub.

## Get your own

```sh
mix $COSMIX/setup.mix --ctl ~/.ctl      # any path; --skip-build to copy only
cd ~/.ctl && $EDITOR _etc/public-hygiene.conf.mix
mix _bin/install-hygiene-hooks.mix        # after listing your repos in the conf
```

It is a **copy**, not a link — from that moment the hub is yours. It needs
no git and no GitHub account: `git init` there later if you want history,
and push it to a private remote only if and when you want one. Updates to
the starter flow to your copy by diff, never automatically. Every script in
it locates the hub from its own path, so the copy works wherever you put it.

## What is in it

| Path | What | Yours to |
|---|---|---|
| `CLAUDE.md`, `CODEX.md` | the project mandate and working map an agent reads first | edit; add your mesh-private addendum under `_doc/` and `@`-import it |
| `_doc/` | the working method (ultracode workflows, convergence review) | extend |
| `_spec/` | the specification suite (21 of 28 chapters — see below) | read; propose changes upstream |
| `_decisions/` | architecture decision records (38 of 45) | read; add your own |
| `_plan/`, `_journal/` | empty; date-prefixed `YYYY-MM-DD-title.md` files go here | fill |
| `_lib/deploy_lib.mix` | the shared half of every deploy script: hub + `$COSMIX` locators, roster, inventory lookups (`mesh_ip`, hubs-last `rollout_order`), snapshot, version compare + downgrade refusal, `shipped()` asset resolution | require it; never re-implement it |
| `_lib/mesh_lib.mix` | signed-inventory verification, WG/zone generation, the mesh write lock | require it |
| `_lib/provisiond_worker_lib.mix`, `_lib/sshm_worker_lib.mix` | the payload halves of the provisioning and ssh-manager workers, shipped to nodes by their deploy scripts | require it |
| `_bin/*.mix` (118) | every operational script of the reference hub: 27 fleet deploys + provisioning, the mesh tooling (`mesh_*`, WG/zone/registry), the build-cluster drivers (`cbc-*`), mail/DAV operations, web-palette gates, release + dev tools, and `test-public-hygiene.mix` (the gate's own 415-probe suite). Every remote step is `ssh_mix`, every local one `run_argv`; each refuses clearly until its `_etc/<name>.conf.mix` exists | copy each `.example` you need, fill it, run |
| `_etc/*.conf.mix.example` (91) | one template per script that needs site facts (nodes, URLs, paths) — the things that never ship | copy, fill, keep private |
| `_bin/check-public-hygiene.mix` | the public-hygiene gate: refuses commits/pushes carrying private identity | run; never widen its allowlist to get past it |
| `_bin/install-hygiene-hooks.mix` | installs the gate as pre-commit/pre-push in each listed repo | run after editing the conf |
| `_bin/gen-versions.mix` | regenerates `$COSMIX/docs/VERSIONS.md` from the manifests | run at release time |
| `_etc/public-hygiene.conf.mix` | **edit first**: your repos, your domains, your home path | edit |
| `_etc/public-hygiene.allow.conf.mix` | standing exceptions, by content fingerprint; ships empty | leave empty until a finding is inspected |

**Site assets.** A unit file, sysusers/tmpfiles fragment, polkit rule or config
payload is the public template under `$COSMIX/src/_etc` unless your hub carries
its own copy under `_etc/systemd`, `_etc/sysusers`, `_etc/tmpfiles`,
`_etc/polkit-1/rules.d` or `_etc/cosmix` — then that copy wins (`shipped()` in
`deploy_lib`). A unit that names one of your addresses is config, not template;
keep it in the hub.

**Mesh inventory.** The gate derives node-name and subnet rules from
`_etc/mesh/inventory.mix` + `inventory.signed` when they exist. A hub with no
mesh has neither; the gate then runs your static rules only and says so on
every scan. The inventory format, signing, and the deploy scripts that use
it are the next things to graduate into this starter (they still carry the
original operator's hosts, so they stay private for now).

## Not in the starter yet

Seven spec chapters and seven decision records still name the original
operator's nodes or domains and are held back until sanitised:
`_spec/` 01-bus-wire-protocol, 06-display-model, 10-daemon-identity,
11-netserva-package-install, 12c-authz-transport-audit,
13-mesh-architecture, CHANGELOG; `_decisions/` 2026-05-09 mesh-hostname
resolution, 2026-05-15 federated addressing, 2026-06-09 knowledge axes,
2026-06-18 css base-site methodology, 2026-07-15 ct appliance vision,
2026-07-25 no-hardwired-mesh-values, 2026-08-15 loose node identity. The
same gate that guards this folder is what decides when they can come in.
