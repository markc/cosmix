# cosmix-maild command line

## Synopsis

```text
cosmix-maild [--config PATH] <command>
```

`--version` prints the crate version, source revision, and build time.

Most administration commands are transient Bus clients of the running daemon. `serve`, `migrate`, and `queue` use the selected local configuration and storage directly.

## Commands

### Daemon and storage

| Command | Effect |
|---|---|
| `serve` | Build the runtime and serve HTTP, SMTP, and configured IMAP listeners |
| `migrate` | Open the configured database and apply migrations |
| `queue list` | Show up to 50 queued outbound messages |
| `queue flush` | Make queued messages immediately eligible for retry |

### Accounts

| Command | Effect |
|---|---|
| `account add EMAIL [PASSWORD] [--name NAME] [--stdin] [--force]` | Create a complete account row |
| `account passwd EMAIL [--stdin]` | Patch only the password |
| `account verify EMAIL [--stdin]` | Test a password without changing state |
| `account lock EMAIL` | Disable password authentication while preserving the hash |
| `account unlock EMAIL` | Restore the password retained by `lock` |
| `account mfa EMAIL STATE` | Set `mfa_enabled`; accepts on/off, true/false, yes/no, or 1/0 |
| `account list` | List account addresses and display names |
| `account show EMAIL` | Show the complete property record with secret redaction |
| `account delete EMAIL` | Delete an account |
| `account seed-mailboxes [--email EMAIL]` | Idempotently create default JMAP mailboxes for one or all accounts |

`account add` is a full replacement. It refuses an existing address unless `--force` is supplied because replacement resets omitted account settings to the command defaults. Use `account passwd` for a password-only change.

If no password argument is supplied, `account add` prompts without echo and asks for confirmation. `account passwd` always prompts unless `--stdin` is used. A positional password is retained for scripted compatibility but is visible in process listings and shell history.

`account verify` exits successfully for a valid password and with status 1 for an invalid password. Unknown accounts also report invalid, avoiding account enumeration through this command.

Example:

```text
cosmix-maild account add admin@example.com --name "Example Administrator"
cosmix-maild account passwd admin@example.com
cosmix-maild account lock admin@example.com
cosmix-maild account unlock admin@example.com
```

### Per-account rule overrides

| Command | Effect |
|---|---|
| `account-overrides get --email EMAIL` | Read an override row |
| `account-overrides set --email EMAIL [OPTIONS]` | Replace or patch an override row |
| `account-overrides clear --email EMAIL` | Remove the row and restore engine defaults |

`set` accepts:

- `--threshold NUMBER`
- repeatable `--disable-rule RULE_ID`
- repeatable `--allow GLOB`
- repeatable `--block GLOB`
- `--merge` for patch semantics
- `--force` to omit the prior-read version check in patch mode

Without `--merge`, omitted fields return to schema defaults. With `--merge`, unspecified fields remain unchanged.

### Domains

| Command | Effect |
|---|---|
| `domain add FQDN` | Create a domain row with schema defaults |
| `domain list` | List domain, role, and enabled state |
| `domain show FQDN` | Show the complete domain row |
| `domain set FQDN FIELD VALUE` | Patch one field |
| `domain remove FQDN` | Remove a domain row |

`VALUE` is parsed as JSON first and falls back to a plain string. This permits booleans, `null`, arrays, and unquoted string arguments.

```text
cosmix-maild domain add example.com
cosmix-maild domain set example.com enabled true
cosmix-maild domain set example.com mx_targets '["mx.example.com"]'
cosmix-maild domain show example.com
```

### Aliases

| Command | Effect |
|---|---|
| `alias add ALIAS TARGET` | Create a local single-hop alias |
| `alias list` | List alias mappings and enabled state |
| `alias remove ALIAS` | Remove an alias idempotently |

The target must be a real local account and cannot itself be an alias. Account and alias addresses are canonicalised before the Bus call.

```text
cosmix-maild alias add info@example.com admin@example.com
```

### TLS and DKIM

| Command | Effect |
|---|---|
| `tls-identity list` | List projected TLS identities |
| `tls-identity show SERVER_NAME` | Show one projected identity |
| `tls reload` | Rebuild and atomically swap the live SNI resolver |
| `dkim generate --domain DOMAIN --selector SELECTOR [--algorithm ALGORITHM]` | Generate a substrate-managed key and print its DNS TXT record |
| `dkim rotate --domain DOMAIN --selector SELECTOR` | Promote an existing selector |
| `dkim retire --domain DOMAIN --selector SELECTOR` | Remove a non-active selector |

`tls-identity` is read-only. `tls reload` merges startup identities with substrate-declared identities, reloads PEM files, updates the projection, and keeps the previous resolver active if rebuilding fails.

The default DKIM algorithm is `rsa-sha256`; `ed25519-sha256` is also accepted. `rotate` does not generate a key. An active selector must be replaced before it can be retired.

### Engine and classifiers

| Command | Effect |
|---|---|
| `engine-config show` | Show the `maild.engine_config` singleton |
| `rules stats [--top-n N]` | Show pack metadata, verdict totals, and rule hits |
| `rules reload` | Re-read and atomically swap the configured rule pack |
| `bayesian stats ACCOUNT_ID` | Show corpus statistics for a numeric account id |

`rules stats` defaults to 256 per-rule entries. The daemon caps the request at 4096. `--top-n 0` requests cardinality without cloning the per-rule map.

`rules reload` is a no-op when `rules_pack_path` is unset. On a load failure, the previous pack remains active.

The CLI does not expose `maild.rules.explain` or `maild.bayesian.classify`; their base64 message and envelope payloads are intended for direct Bus callers.

## Configuration selection

`--config PATH` loads that strict-data `.conf.mix` file. Without it, the binary checks the system path, the Cosmix user path, node configuration, and then built-in defaults.

The config is loaded before every command. Bus-only commands still require a valid selected configuration even though their mutations occur through the running daemon.

## Bus-only command behaviour

Account, override, alias, domain, TLS-identity, engine-config, DKIM, rules, Bayesian, and TLS commands connect to the Bus broker and call the running `maild` service. They do not edit the daemon database directly.

The daemon must therefore be registered and reachable. Mutations pass through property validation, audit, lifecycle hooks, and change publication.

