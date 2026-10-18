# mix-scripted CMS on webd (DB-from-Mix)

A minimal, multi-tenant CMS written as a Mix handler. Each vhost is a
tenant with its own SQLite database; an operator-authored `.mix` handler
reads and writes that database via the `db_query`/`db_exec` builtins,
scoped to the vhost by webd. This is the trusted in-process embed (slice
#3.5) — the handler runs inside webd with a least-privilege sandbox.

## What you get

* **`cms.mix`** — one handler: GET renders the post list + a create form;
  POST inserts a post and redirects (POST-redirect-GET).
* **`layout.mix`** — a shared `page($title, $body)` wrapper that `cms.mix`
  pulls in with `include "layout.mix"`. The embed resolves `include`
  relative to the handler (under `www_dir`), so co-located handlers share
  one layout/partials lib — the layouts pattern, no copy/paste.

## The DB-from-Mix surface

Available to a handler whose route declares `capabilities = ["db"]`:

| call | returns |
|---|---|
| `db_query(sql, params)` | a list of `{ column: value }` rows (use for `SELECT`) |
| `db_exec(sql, params)` | `{ affected, last_insert_id }` (use for INSERT/UPDATE/DELETE/DDL) |
| `url_decode(s)` / `url_encode(s)` | percent/`+` coding for form + query input |

* `params` is a list of positional bind values for `?1`, `?2`, … — always
  **bound**, never string-interpolated, so request input can't SQL-inject.
* The connection is the vhost's own SQLite (the same one the `/api/posts`
  API uses). A handler can only ever reach **its own tenant's** database.
* Request globals injected by webd: `$METHOD` `$PATH` `$QUERY` `$HOST`
  `$BODY` (strings) and `$HEADERS` (map).
* Response contract: return an HTML string (→ `200 text/html`), a
  `{ status, headers, body }` map (typed), or `print(...)` (stdout → body).
* Sandbox for a `db` route: `Pure` + `FsRead` + `Db` only — no raw
  filesystem writes, no network, no process/shell (`sh`/`$()`/pipes are
  denied), plus a recursion cap, a 5 s deadline, and collection-size caps.
  JSON request bodies: use the `json_parse` builtin.

## Deploy it

1. **Give the vhost a database.** In the vhost's `[[webd.vhost]]` config
   block set `cms_db_path = "/var/lib/cosmix/webd/<fqdn>/cms.db"` (any
   path webd can write); restart webd. (Runtime provisioning via the
   `webd.vhosts` namespace is a planned follow-up.)

2. **Copy the handler** under the vhost's `www_dir`, e.g.
   `…/www/<fqdn>/handlers/cms.mix`.

3. **Register the route** on the `webd.handlers` SPEC-12 namespace (the
   standard `webd.props.set` surface; `require_version` → `if_version = 0`
   to create). The row:

   ```
   namespace = handlers
   key       = blog-cms                 # route_id (your label)
   body:
     route_id     = "blog-cms"
     vhost_fqdn   = "<fqdn>"            # the vhost this serves
     method       = "ANY"               # cms.mix handles GET + POST
     path_pattern = "/posts"            # exact, or a trailing glob "/x/*"
     handler_kind = "mix"
     handler_ref  = "handlers/cms.mix"  # relative to www_dir, no '..'
     enabled      = true
     capabilities = ["db"]              # grants the Db capability
   ```

   A route WITHOUT `capabilities = ["db"]` runs read-only (no DB); calling
   `db_query` from it is denied (→ 500). A `db` route on a vhost with no
   `cms_db_path` gets a clean "database not available" error.

4. `curl https://<fqdn>/posts` — you should see the (empty) list + form;
   submit the form to create a post.

## How it stays correct across the trust boundary

The handler calls `db_query`/`db_exec` and never holds a connection. The
seam is async, so the **same script** runs unchanged if a future
untrusted/customer-uploaded tier (slice #4) executes it out-of-process —
there, the worker's DB handler RPCs the query back to webd instead of
touching a local connection. This example is the trusted path.

## Follow-ups (not yet wired)

* **Runtime CMS-vhost provisioning** (a `cms_db_path` column on the
  `webd.vhosts` namespace) so a vhost gets a database without a config
  edit + restart.
* **Slice #4** — out-of-process pooled workers for *untrusted*
  (customer-uploaded) Mix, when that tier appears.
