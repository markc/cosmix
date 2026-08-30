//! `WebdDbHandler` — the [`cosmix_mix::DbHandler`] implementation that
//! backs `db_query`/`db_exec` in an embedded Mix handler with the
//! request's **per-vhost** SQLite connection.
//!
//! Scope is enforced by webd, not the script: the handler is constructed
//! around exactly one vhost's `Arc<Mutex<Connection>>` (the same
//! connection the `/api/posts` CMS API uses), so a Mix script can only
//! ever touch its own tenant's database — it never names a path or holds
//! a connection. Arbitrary SQL is allowed (handlers are trusted,
//! operator-authored — slice #3.5); parameters are always **bound**
//! (never interpolated), so a handler that takes request input still
//! can't SQL-inject itself.
//!
//! Forward-compatibility (slice #4): an out-of-process untrusted worker
//! would implement the same `DbHandler` trait by RPC-ing the query back
//! to webd; the Mix script is identical. This in-process handler is the
//! trusted-path implementation of that seam.

use std::collections::BTreeSet;
use std::sync::Arc;

use cosmix_mix::value::Value;
use cosmix_mix::{DbFuture, DbHandler, MixError, MixResult};
use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use tokio::sync::Mutex;

/// Per-vhost database handler. Holds the same `Arc<Mutex<Connection>>`
/// as the vhost's CMS API, so embedded Mix shares one connection
/// (serialized) with the rest of the vhost.
///
/// `allowed_dbs` is the exact set of database (schema) names this route may
/// touch: `main` iff the route holds the plain `db` capability, plus one entry
/// per `db-schema:<name>` grant for an ATTACHed aux database. A per-statement
/// SQLite authorizer denies every access naming a schema outside this set, so on
/// the *shared* connection: a route with only `db-schema:X` cannot reach `main`
/// (no CMS/session tampering), and a co-hosted `db` route cannot reach another
/// family's aux schema. `ATTACH`/`DETACH` from handler SQL are denied outright
/// (they would let a granted route swap an attached schema's backing file and
/// escape the path boundary). `temp` remains available (per-connection scratch;
/// handlers are trusted operator code — cross-tenant temp isolation is a noted
/// follow-up, not the boundary that matters here).
pub(crate) struct WebdDbHandler {
    conn: Arc<Mutex<Connection>>,
    allowed_dbs: Arc<BTreeSet<String>>,
}

impl WebdDbHandler {
    pub(crate) fn new(conn: Arc<Mutex<Connection>>, allowed_dbs: Arc<BTreeSet<String>>) -> Self {
        Self { conn, allowed_dbs }
    }
}

/// Installs a schema-scoping SQLite authorizer for the lifetime of a single
/// statement and clears it on drop — always, including error paths — so the
/// authorizer never lingers on the shared connection for a co-tenant handler.
/// The `conn` lock is held for the whole span, so install/clear is race-free.
///
/// The authorizer denies, for handler SQL on the shared connection:
/// - `ATTACH`/`DETACH` (schema-file swapping);
/// - `PRAGMA` (a connection-wide `foreign_keys=OFF` / `query_only=ON` /
///   `locking_mode` would corrupt or stall co-tenant statements);
/// - any access naming a schema NOT in `allowed` (`main` refused unless the
///   route holds `db`; an aux schema unless it holds `db-schema:<that>`);
/// - `temp`, UNLESS the route also has `main` (plain `db`) — a `temp.<table>`
///   shadows an unqualified `main.<table>` (e.g. `SELECT role FROM users`), so a
///   schema-only route must not be able to plant one.
///
/// Other schema-less actions (functions, transaction control, plain SELECT) are
/// allowed. It is NOT a transaction gate: billing handlers issue `BEGIN
/// IMMEDIATE` via `db_exec` today; wedged-transaction protection belongs with
/// the future `db_tx` primitive.
struct AuthGuard<'c> {
    conn: &'c Connection,
}

impl<'c> AuthGuard<'c> {
    /// Fail-closed: if the authorizer can't be installed, the caller aborts the
    /// statement rather than run it unscoped.
    fn install(conn: &'c Connection, allowed: Arc<BTreeSet<String>>) -> MixResult<Self> {
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Attach { .. } | AuthAction::Detach { .. } | AuthAction::Pragma { .. }
            ) {
                return Authorization::Deny;
            }
            match ctx.database_name {
                // `temp` is per-connection scratch that can shadow `main`; tie it
                // to plain-db (main) access so a schema-only route can't use it.
                Some("temp") => {
                    if allowed.contains("main") {
                        Authorization::Allow
                    } else {
                        Authorization::Deny
                    }
                }
                Some(db) if allowed.contains(db) => Authorization::Allow,
                Some(_) => Authorization::Deny,
                // Schema-less actions (functions, transaction control, SELECT of
                // a constant, etc.). Attach/Detach/Pragma are already denied above.
                None => Authorization::Allow,
            }
        }))
        .map_err(|e| rt_err(format!("installing db authorizer failed: {e}")))?;
        Ok(Self { conn })
    }
}

impl Drop for AuthGuard<'_> {
    fn drop(&mut self) {
        // Best-effort clear so the authorizer never lingers for the next
        // (co-tenant) statement on the shared connection.
        let _ = self
            .conn
            .authorizer::<fn(AuthContext<'_>) -> Authorization>(None);
    }
}

impl DbHandler for WebdDbHandler {
    fn query<'a>(&'a self, sql: &'a str, params: &'a [Value]) -> DbFuture<'a, MixResult<Value>> {
        Box::pin(async move {
            let conn = self.conn.lock().await;
            let _auth = AuthGuard::install(&conn, Arc::clone(&self.allowed_dbs))?;
            run_query(&conn, sql, params)
        })
    }

    fn exec<'a>(&'a self, sql: &'a str, params: &'a [Value]) -> DbFuture<'a, MixResult<Value>> {
        Box::pin(async move {
            let conn = self.conn.lock().await;
            let _auth = AuthGuard::install(&conn, Arc::clone(&self.allowed_dbs))?;
            run_exec(&conn, sql, params)
        })
    }
}

fn rt_err(msg: impl Into<String>) -> MixError {
    MixError::RuntimeError {
        msg: msg.into(),
        span: None,
    }
}

/// Map a Mix `Value` to an owned rusqlite bind value. Collections /
/// functions can't be a single SQL parameter — reject them clearly.
fn bind_value(v: &Value) -> Result<rusqlite::types::Value, MixError> {
    use rusqlite::types::Value as Sv;
    Ok(match v {
        Value::Nil => Sv::Null,
        Value::Bool(b) => Sv::Integer(i64::from(*b)),
        Value::Number(n) => {
            let n = *n;
            // Bind as INTEGER only when the value is a finite integral
            // f64 within i64 range; otherwise bind as REAL so a large
            // magnitude (e.g. 1e20) doesn't saturate to i64::MAX.
            // `i64::MIN as f64` (-2^63) and `i64::MAX as f64` (2^63) are
            // exact in f64; `n < 2^63` keeps `n as i64` in range.
            if n.is_finite() && n.fract() == 0.0 && n >= (i64::MIN as f64) && n < (i64::MAX as f64)
            {
                Sv::Integer(n as i64)
            } else {
                Sv::Real(n)
            }
        }
        Value::String(s) => Sv::Text(s.clone()),
        Value::Bytes(b) => Sv::Blob(b.to_vec()),
        other => {
            return Err(rt_err(format!(
                "cannot bind a {} as a SQL parameter (use a scalar: string/number/bool/nil/bytes)",
                other.type_name()
            )));
        }
    })
}

fn bind_all(params: &[Value]) -> Result<Vec<rusqlite::types::Value>, MixError> {
    params.iter().map(bind_value).collect()
}

/// One SQLite cell → a Mix `Value`, built directly (no serde_json hop)
/// so binary blobs survive as `Value::Bytes` and a non-finite REAL stays
/// a `Value::Number` distinct from SQL NULL (`Value::Nil`). Integers are
/// widened to `f64` (Mix's only numeric type) — lossy beyond 2^53, an
/// inherent Mix-number limit, not specific to this path.
fn cell_to_value(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef as Vr;
    match v {
        Vr::Null => Value::Nil,
        Vr::Integer(i) => Value::Number(i as f64),
        Vr::Real(f) => Value::Number(f),
        Vr::Text(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
        Vr::Blob(b) => Value::bytes(b.to_vec()),
    }
}

/// `db_query`: run `sql` and return a `Value::List` of `Value::Map` rows
/// (column name → value, insertion-ordered). Suitable for `SELECT`.
fn run_query(conn: &Connection, sql: &str, params: &[Value]) -> MixResult<Value> {
    let bound = bind_all(params)?;
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| rt_err(format!("prepare failed: {e}")))?;
    let ncols = stmt.column_count();
    let col_names: Vec<String> = (0..ncols)
        .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
        .collect();
    let mut out: Vec<Value> = Vec::new();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(bound.iter()))
        .map_err(|e| rt_err(format!("query failed: {e}")))?;
    while let Some(row) = rows
        .next()
        .map_err(|e| rt_err(format!("row read failed: {e}")))?
    {
        let mut m: cosmix_mix::IndexMap<String, Value> = cosmix_mix::IndexMap::new();
        for (i, name) in col_names.iter().enumerate() {
            let cell = row
                .get_ref(i)
                .map_err(|e| rt_err(format!("column {name} read failed: {e}")))?;
            m.insert(name.clone(), cell_to_value(cell));
        }
        out.push(Value::map(m));
    }
    Ok(Value::list(out))
}

/// `db_exec`: run a single mutating/DDL statement; return a `Value::Map`
/// `{ affected, last_insert_id }`.
fn run_exec(conn: &Connection, sql: &str, params: &[Value]) -> MixResult<Value> {
    let bound = bind_all(params)?;
    let affected = conn
        .execute(sql, rusqlite::params_from_iter(bound.iter()))
        .map_err(|e| rt_err(format!("exec failed: {e}")))?;
    let mut m: cosmix_mix::IndexMap<String, Value> = cosmix_mix::IndexMap::new();
    m.insert("affected".into(), Value::Number(affected as f64));
    m.insert(
        "last_insert_id".into(),
        Value::Number(conn.last_insert_rowid() as f64),
    );
    Ok(Value::map(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, n INTEGER, r REAL, b BLOB);",
        )
        .unwrap();
        Arc::new(Mutex::new(conn))
    }

    /// A plain `db` route: `main` allowed, no aux schemas (the common case).
    fn handler(conn: Arc<Mutex<Connection>>) -> WebdDbHandler {
        WebdDbHandler::new(conn, Arc::new(BTreeSet::from(["main".to_string()])))
    }

    #[tokio::test]
    async fn exec_then_query_roundtrips() {
        let h = handler(mem_db());
        let r = h
            .exec(
                "INSERT INTO t (name, n, r) VALUES (?1, ?2, ?3)",
                &[
                    Value::String("hi".into()),
                    Value::Number(7.0),
                    Value::Number(1.5),
                ],
            )
            .await
            .unwrap();
        // {affected: 1, last_insert_id: 1}
        match &r {
            Value::Map(m) => {
                assert_eq!(m.get("affected").and_then(Value::to_number), Some(1.0));
                assert_eq!(
                    m.get("last_insert_id").and_then(Value::to_number),
                    Some(1.0)
                );
            }
            other => panic!("expected map, got {other:?}"),
        }
        let rows = h.query("SELECT name, n, r FROM t", &[]).await.unwrap();
        match &rows {
            Value::List(rows) => {
                assert_eq!(rows.len(), 1);
                match &rows[0] {
                    Value::Map(m) => {
                        assert_eq!(m.get("name"), Some(&Value::String("hi".into())));
                        assert_eq!(m.get("n").and_then(Value::to_number), Some(7.0));
                        assert_eq!(m.get("r").and_then(Value::to_number), Some(1.5));
                    }
                    other => panic!("expected row map, got {other:?}"),
                }
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn params_are_bound_not_interpolated() {
        let h = handler(mem_db());
        // A classic injection payload as a bound param must be stored
        // verbatim, not executed.
        let payload = "x'); DROP TABLE t;--";
        h.exec(
            "INSERT INTO t (name) VALUES (?1)",
            &[Value::String(payload.into())],
        )
        .await
        .unwrap();
        let rows = h.query("SELECT name FROM t", &[]).await.unwrap();
        match &rows {
            Value::List(rows) => {
                assert_eq!(rows.len(), 1, "table still exists; row stored verbatim");
                if let Value::Map(m) = &rows[0] {
                    assert_eq!(m.get("name"), Some(&Value::String(payload.into())));
                }
            }
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_sql_is_error_not_panic() {
        let h = handler(mem_db());
        let err = h.query("SELECT * FROM nonexistent", &[]).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn blob_column_roundtrips_as_bytes_not_lossy_text() {
        let h = handler(mem_db());
        // Bytes that are NOT valid UTF-8 — a lossy-string mapping would
        // corrupt them; Value::Bytes preserves them exactly.
        let raw = vec![0xffu8, 0x00, 0xfe, 0x9f];
        h.exec(
            "INSERT INTO t (b) VALUES (?1)",
            &[Value::bytes(raw.clone())],
        )
        .await
        .unwrap();
        let rows = h.query("SELECT b FROM t", &[]).await.unwrap();
        match &rows {
            Value::List(rows) => match &rows[0] {
                Value::Map(m) => assert_eq!(m.get("b"), Some(&Value::bytes(raw))),
                other => panic!("expected row map, got {other:?}"),
            },
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn null_is_nil_not_confused_with_other_scalars() {
        let h = handler(mem_db());
        h.exec("INSERT INTO t (name) VALUES (NULL)", &[])
            .await
            .unwrap();
        let rows = h.query("SELECT name, n FROM t", &[]).await.unwrap();
        if let Value::List(rows) = &rows
            && let Value::Map(m) = &rows[0]
        {
            assert_eq!(m.get("name"), Some(&Value::Nil));
            assert_eq!(m.get("n"), Some(&Value::Nil));
        }
    }

    /// A connection with one attached aux schema `sshm` holding table `hosts`.
    fn mem_db_with_aux() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);\n\
             ATTACH DATABASE ':memory:' AS sshm;\n\
             CREATE TABLE sshm.hosts (id INTEGER PRIMARY KEY, name TEXT);\n\
             INSERT INTO sshm.hosts (name) VALUES ('alpha');",
        )
        .unwrap();
        Arc::new(Mutex::new(conn))
    }

    /// A route granted `main` plus the named aux schemas (the shape a real sshm
    /// route has: plain `db` + `db-schema:sshm`).
    fn granting(conn: Arc<Mutex<Connection>>, names: &[&str]) -> WebdDbHandler {
        let mut set: BTreeSet<String> = names.iter().map(|s| s.to_string()).collect();
        set.insert("main".to_string());
        WebdDbHandler::new(conn, Arc::new(set))
    }

    /// A route holding ONLY `db-schema:X` (aux schema, no `main`).
    fn schema_only(conn: Arc<Mutex<Connection>>, names: &[&str]) -> WebdDbHandler {
        let set: BTreeSet<String> = names.iter().map(|s| s.to_string()).collect();
        WebdDbHandler::new(conn, Arc::new(set))
    }

    #[tokio::test]
    async fn ungranted_handler_denied_on_attached_schema() {
        // No db-schema grant: reads/writes to sshm.* must be refused even
        // though the schema is attached on the shared connection.
        let db = mem_db_with_aux();
        let h = handler(Arc::clone(&db));
        assert!(h.query("SELECT name FROM sshm.hosts", &[]).await.is_err());
        assert!(
            h.exec("INSERT INTO sshm.hosts (name) VALUES ('x')", &[])
                .await
                .is_err()
        );
        // main is still reachable for the same (ungranted-for-aux) handler.
        h.query("SELECT id FROM t", &[]).await.unwrap();
    }

    #[tokio::test]
    async fn schema_only_route_cannot_reach_main() {
        // A route with only `db-schema:sshm` (no plain `db`) reaches sshm but is
        // denied `main` — it cannot read or tamper with CMS/session tables.
        let db = mem_db_with_aux();
        let h = schema_only(Arc::clone(&db), &["sshm"]);
        h.query("SELECT name FROM sshm.hosts", &[]).await.unwrap();
        assert!(
            h.query("SELECT id FROM t", &[]).await.is_err(),
            "main denied without db cap"
        );
        assert!(
            h.exec("INSERT INTO t (name) VALUES ('x')", &[])
                .await
                .is_err(),
            "main write denied without db cap"
        );
    }

    #[tokio::test]
    async fn handler_cannot_attach_or_detach() {
        // ATTACH/DETACH from handler SQL are refused (schema-file swapping).
        let db = mem_db_with_aux();
        let h = granting(Arc::clone(&db), &["sshm"]);
        assert!(
            h.exec("DETACH DATABASE sshm", &[]).await.is_err(),
            "DETACH must be denied"
        );
        assert!(
            h.exec("ATTACH DATABASE ':memory:' AS evil", &[])
                .await
                .is_err(),
            "ATTACH must be denied"
        );
    }

    #[tokio::test]
    async fn handler_cannot_issue_pragma() {
        // A connection-wide PRAGMA would corrupt co-tenant statements.
        let h = handler(mem_db());
        assert!(
            h.exec("PRAGMA foreign_keys=OFF", &[]).await.is_err(),
            "PRAGMA must be denied even for a plain-db route"
        );
    }

    #[tokio::test]
    async fn schema_only_route_cannot_use_temp() {
        // A schema-only route must not create a temp table that shadows main
        // (e.g. temp.users overriding an unqualified `SELECT ... FROM users`).
        let db = mem_db_with_aux();
        let h = schema_only(Arc::clone(&db), &["sshm"]);
        assert!(
            h.exec("CREATE TEMP TABLE users (id INTEGER, role TEXT)", &[])
                .await
                .is_err(),
            "temp DDL must be denied without plain db"
        );
        // A plain-db route (has main) may use temp.
        let h2 = granting(Arc::clone(&db), &["sshm"]);
        h2.exec("CREATE TEMP TABLE scratch (id INTEGER)", &[])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn granted_handler_reaches_its_schema_only() {
        let db = mem_db_with_aux();
        let h = granting(Arc::clone(&db), &["sshm"]);
        let rows = h.query("SELECT name FROM sshm.hosts", &[]).await.unwrap();
        match &rows {
            Value::List(rows) => {
                assert_eq!(rows.len(), 1);
                if let Value::Map(m) = &rows[0] {
                    assert_eq!(m.get("name"), Some(&Value::String("alpha".into())));
                }
            }
            other => panic!("expected list, got {other:?}"),
        }
        // A grant for "sshm" does not open some other attached name.
        let h2 = granting(Arc::clone(&db), &["other"]);
        assert!(h2.query("SELECT name FROM sshm.hosts", &[]).await.is_err());
    }

    #[tokio::test]
    async fn authorizer_cleared_after_call_leaves_connection_open() {
        // After a granted query returns, a subsequent ungranted handler on the
        // same connection must see the default-deny again (authorizer cleared).
        let db = mem_db_with_aux();
        granting(Arc::clone(&db), &["sshm"])
            .query("SELECT name FROM sshm.hosts", &[])
            .await
            .unwrap();
        let h = handler(Arc::clone(&db));
        assert!(h.query("SELECT name FROM sshm.hosts", &[]).await.is_err());
        // main still works — the connection was not wedged by the authorizer.
        h.query("SELECT id FROM t", &[]).await.unwrap();
    }
}
