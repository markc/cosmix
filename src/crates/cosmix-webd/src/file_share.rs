//! Token catalogue for public file shares, managed and served by shares.rs.
//!
//! Account identity is the exact canonical email in the unified session. Legacy
//! numeric rows survive migration but cannot resolve until explicitly mapped.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::blob_reference::Reference;

const SCHEMA: &str = "
CREATE TABLE file_shares (
    token TEXT PRIMARY KEY,
    primary_fqdn TEXT,
    account TEXT,
    maild_account_id INTEGER,
    rel_path TEXT,
    blob TEXT,
    kind TEXT NOT NULL,
    password_hash TEXT,
    expires_at INTEGER,
    created_at INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0,
    download_count INTEGER NOT NULL DEFAULT 0,
    CHECK ((rel_path IS NOT NULL AND blob IS NULL) OR
           (rel_path IS NULL AND blob IS NOT NULL)),
    CHECK (account IS NOT NULL OR maild_account_id IS NOT NULL)
);";
const INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_file_shares_account
    ON file_shares(account) WHERE revoked = 0;";

/// Initialise or atomically migrate the old numeric-account scaffold.
/// No guesses about email identity: old rows retain their ID with account=NULL.
pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='file_shares')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        tx.execute_batch(SCHEMA)?;
    } else {
        let old: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('file_shares') WHERE name='account_id')",
            [],
            |r| r.get(0),
        )?;
        if old {
            tx.execute_batch(
                &SCHEMA.replace("CREATE TABLE file_shares", "CREATE TABLE file_shares_p5"),
            )?;
            tx.execute_batch(
                "INSERT INTO file_shares_p5
                    (token, account, maild_account_id, rel_path, blob, kind,
                     password_hash, expires_at, created_at, revoked, download_count)
                 SELECT token, NULL, account_id, rel_path, NULL, kind,
                     password_hash, expires_at, created_at, revoked, download_count
                 FROM file_shares;
                 DROP TABLE file_shares;
                 ALTER TABLE file_shares_p5 RENAME TO file_shares;",
            )?;
        }
    }
    let scoped: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('file_shares') WHERE name='primary_fqdn')",
        [], |r| r.get(0),
    )?;
    if !scoped {
        tx.execute_batch("ALTER TABLE file_shares ADD COLUMN primary_fqdn TEXT")?;
    }
    // Unowned legacy rows remain intact but cannot resolve on any primary.
    tx.execute_batch(INDEX)?;
    // Also fail on an incompatible pre-existing table, instead of claiming migration succeeded.
    tx.prepare(
        "SELECT token, account, maild_account_id, rel_path, blob, kind,
        password_hash, expires_at, created_at, revoked, download_count FROM file_shares LIMIT 0",
    )?;
    tx.commit()
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not_found")]
    NotFound,
    #[error("expired")]
    Expired,
    #[error("revoked")]
    Revoked,
    #[error("unauthorized")]
    Unauthorized,
    #[error("invalid_arguments: {0}")]
    InvalidArguments(&'static str),
    #[error("share catalogue unavailable")]
    Database(#[from] rusqlite::Error),
}

/// Only file targets are deliverable this arc. Old unknown/dir/drop rows fail closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Target {
    Path { rel_path: String },
    Blob { reference: Reference },
}

impl Target {
    pub fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Path { rel_path } if valid_relative_path(rel_path) => Ok(()),
            Self::Path { .. } => Err(Error::InvalidArguments("unsafe relative path")),
            Self::Blob { reference } => reference
                .validate()
                .map_err(|_| Error::InvalidArguments("invalid blob reference")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Share {
    pub token: String,
    pub account: String,
    pub maild_account_id: Option<i64>,
    pub target: Target,
    pub kind: String,
    pub has_password: bool,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub revoked: bool,
    pub download_count: i64,
}

/// Exact session identity: do not lowercase the local part or equate CMS IDs
/// with maild IDs. The identity provider owns canonicalisation.
pub fn valid_account(account: &str) -> bool {
    let Some((local, domain)) = account.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && account.len() <= 320
        && !account.chars().any(|c| c.is_control() || c.is_whitespace())
}

pub fn valid_relative_path(rel: &str) -> bool {
    !rel.is_empty()
        && rel.len() <= 4096
        && !rel.contains('\\')
        && !rel.chars().any(char::is_control)
        && rel
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// Validated operator roots, never request data. Handles pin their directory identity.
#[derive(Default)]
pub struct Roots(BTreeMap<String, (std::path::PathBuf, std::sync::Arc<cosmix_files::rooted_read::ReadRoot>)>);

impl Roots {
    pub fn from_config(config: &cosmix_config::node::WebdSharesConfig) -> Self {
        let mut roots = BTreeMap::new();
        for (account, path) in &config.roots {
            let safe = valid_account(account)
                && path.is_absolute()
                && !path.components().any(|c| matches!(c, Component::ParentDir));
            let canonical = safe
                .then(|| path.canonicalize().ok())
                .flatten()
                .and_then(|p| cosmix_files::rooted_read::ReadRoot::open(&p).ok().map(|r| (p, std::sync::Arc::new(r))));
            match canonical {
                Some(path) => {
                    roots.insert(account.clone(), path);
                }
                None => tracing::warn!(account, "ignoring invalid webd.shares root"),
            }
        }
        Self(roots)
    }

    #[cfg(test)]
    pub fn get(
        &self,
        account: &str,
    ) -> Option<&std::sync::Arc<cosmix_files::rooted_read::ReadRoot>> {
        self.0.get(account).map(|(_, root)| root)
    }

    pub fn exclude_public(&mut self, public: &[std::path::PathBuf]) {
        self.0.retain(|account, (path, _)| {
            let safe = !public.iter().any(|p| path.starts_with(p));
            if !safe { tracing::warn!(account, "ignoring share root beneath a public serving directory"); }
            safe
        });
    }

    pub fn checked_get(&self, account: &str, directory: &crate::vhost_directory::VhostDirectory)
        -> Option<std::sync::Arc<cosmix_files::rooted_read::ReadRoot>> {
        let (path, root) = self.0.get(account)?;
        // Re-evaluate the current snapshot on every access, including after reload.
        if public_roots(directory).iter().any(|p| path.starts_with(p)) {
            tracing::warn!(account, "refusing share root beneath a public serving directory after reload");
            return None;
        }
        Some(root.clone())
    }
}

pub fn public_roots(directory: &crate::vhost_directory::VhostDirectory) -> Vec<std::path::PathBuf> {
    directory.primaries.iter().flat_map(|p| {
        std::iter::once(&p.state.www_dir).chain(p.state.docs_dir.iter())
    }).filter_map(|p| p.canonicalize().ok()).collect()
}

pub fn mint_token() -> String {
    let mut buf = [0u8; 20];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

pub fn valid_token(token: &str) -> bool {
    token.len() == 27
        && token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Caller checks path ownership or establishes the blob pin BEFORE publishing.
/// Password hashing is performed outside the DB lock by the management layer.
#[allow(clippy::too_many_arguments)]
pub fn create(
    conn: &Connection,
    primary: &str,
    account: &str,
    kind: &str,
    target: &Target,
    password_hash: Option<&str>,
    expires_at: Option<i64>,
    now: i64,
) -> Result<String, Error> {
    if !valid_account(account) {
        return Err(Error::InvalidArguments("invalid account"));
    }
    if kind != "file" {
        return Err(Error::InvalidArguments(
            "only file shares are supported; dir and drop are deferred",
        ));
    }
    target.validate()?;
    if expires_at.is_some_and(|e| e <= now) {
        return Err(Error::InvalidArguments("expires must be in the future"));
    }
    let token = mint_token();
    let (path, blob) = match target {
        Target::Path { rel_path } => (Some(rel_path.as_str()), None),
        Target::Blob { reference } => (
            None,
            Some(
                serde_json::to_string(reference)
                    .map_err(|_| Error::InvalidArguments("invalid blob reference"))?,
            ),
        ),
    };
    conn.execute(
        "INSERT INTO file_shares
         (token, account, rel_path, blob, kind, password_hash, expires_at, created_at, primary_fqdn)
         VALUES (?1, ?2, ?3, ?4, 'file', ?5, ?6, ?7, ?8)",
        params![token, account, path, blob, password_hash, expires_at, now, primary],
    )?;
    Ok(token)
}

struct Row {
    token: String,
    account: Option<String>,
    maild_account_id: Option<i64>,
    rel_path: Option<String>,
    blob: Option<String>,
    kind: String,
    password_hash: Option<String>,
    expires_at: Option<i64>,
    created_at: i64,
    revoked: bool,
    download_count: i64,
}

impl Row {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            token: row.get("token")?,
            account: row.get("account")?,
            maild_account_id: row.get("maild_account_id")?,
            rel_path: row.get("rel_path")?,
            blob: row.get("blob")?,
            kind: row.get("kind")?,
            password_hash: row.get("password_hash")?,
            expires_at: row.get("expires_at")?,
            created_at: row.get("created_at")?,
            revoked: row.get::<_, i64>("revoked")? != 0,
            download_count: row.get("download_count")?,
        })
    }

    fn share(self) -> Result<Share, Error> {
        let account = self
            .account
            .filter(|a| valid_account(a))
            .ok_or(Error::NotFound)?;
        if self.kind != "file" {
            return Err(Error::NotFound);
        }
        let target = match (self.rel_path, self.blob) {
            (Some(rel_path), None) => Target::Path { rel_path },
            (None, Some(blob)) => {
                let value = serde_json::from_str(&blob).map_err(|_| Error::NotFound)?;
                Target::Blob {
                    reference: Reference::from_json(&value).map_err(|_| Error::NotFound)?,
                }
            }
            _ => return Err(Error::NotFound),
        };
        target.validate().map_err(|_| Error::NotFound)?;
        Ok(Share {
            token: self.token,
            account,
            maild_account_id: self.maild_account_id,
            target,
            kind: self.kind,
            has_password: self.password_hash.is_some(),
            expires_at: self.expires_at,
            created_at: self.created_at,
            revoked: self.revoked,
            download_count: self.download_count,
        })
    }
}

/// Bounded, cursor-paginated management inventory. Password hashes never leave it.
pub fn list(
    conn: &Connection,
    primary: &str,
    account: &str,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<Share>, Error> {
    if !valid_account(account) || !(1..=100).contains(&limit) {
        return Err(Error::InvalidArguments(
            "invalid account or list limit (1..100)",
        ));
    }
    let mut stmt = conn.prepare(
        "SELECT * FROM file_shares WHERE account=?1 AND revoked=0 AND token>?2 AND primary_fqdn=?4 ORDER BY token LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![account, after.unwrap_or(""), limit as i64, primary],
        Row::read,
    )?;
    rows.map(|row| row.map_err(Error::from)?.share()).collect()
}

pub fn revoke(conn: &Connection, primary: &str, account: &str, token: &str) -> Result<bool, Error> {
    Ok(conn.execute(
        "UPDATE file_shares SET revoked=1 WHERE token=?1 AND account=?2 AND revoked=0 AND primary_fqdn=?3",
        params![token, account, primary],
    )? > 0)
}

/// Snapshot the gate without filesystem/lane access. The caller drops the DB
/// lock before bounded bcrypt work, then reloads this gate before target access.
pub struct Gate {
    pub share: Share,
    pub password_hash: Option<String>,
}

pub fn resolve(conn: &Connection, primary: &str, token: &str, now: i64) -> Result<Gate, Error> {
    if !valid_token(token) {
        return Err(Error::NotFound);
    }
    let row = conn
        .query_row(
            "SELECT * FROM file_shares WHERE token=?1 AND primary_fqdn=?2",
            params![token, primary],
            Row::read,
        )
        .optional()?
        .ok_or(Error::NotFound)?;
    if row.revoked {
        return Err(Error::Revoked);
    }
    if row.expires_at.is_some_and(|e| now >= e) {
        return Err(Error::Expired);
    }
    let password_hash = row.password_hash.clone();
    Ok(Gate {
        share: row.share()?,
        password_hash,
    })
}

impl Gate {
    /// A bcrypt result is accepted only for the exact hash that was checked.
    /// Re-resolve first so revocation/expiry/password changes cannot race the gate.
    pub fn authorize(&self, verified_hash: Option<&str>) -> Result<&Target, Error> {
        if let Some(expected) = &self.password_hash
            && verified_hash != Some(expected.as_str())
        {
            return Err(Error::Unauthorized);
        }
        Ok(&self.share.target)
    }
}

/// Best-effort download-start telemetry, invoked once on first emitted body bytes.
/// HEAD, denial and empty-body responses do not increment.
pub fn bump_download(conn: &Connection, token: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE file_shares SET download_count=download_count+1 WHERE token=?1",
        [token],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    const ACCOUNT: &str = "user@example.test";

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        init_schema(&db).unwrap();
        db
    }
    fn path() -> Target {
        Target::Path {
            rel_path: "Docs/report.pdf".into(),
        }
    }
    fn blob() -> Target {
        Target::Blob {
            reference: Reference {
                blob: format!("b3:{}", "a".repeat(64)),
                size: 3,
                mime: "text/plain".into(),
                name: Some("file.txt".into()),
                origin: "alpha".into(),
            },
        }
    }

    #[test]
    fn shared_database_isolates_primary_tokens_and_management() {
        let db = db();
        let token = create(&db, "a.example", ACCOUNT, "file", &path(), None, None, 1).unwrap();
        assert!(matches!(resolve(&db, "b.example", &token, 2), Err(Error::NotFound)));
        assert!(list(&db, "b.example", ACCOUNT, None, 100).unwrap().is_empty());
        assert!(!revoke(&db, "b.example", ACCOUNT, &token).unwrap());
        assert!(resolve(&db, "a.example", &token, 2).is_ok());
    }

    #[test]
    fn fresh_schema_target_check_and_repeated_init() {
        let db = db();
        init_schema(&db).unwrap();
        for target in [path(), blob()] {
            let token = create(&db, "a.example",  ACCOUNT, "file", &target, None, None, 1).unwrap();
            assert_eq!(resolve(&db, "a.example",  &token, 2).unwrap().share.target, target);
            assert!(
                db.execute(
                    "UPDATE file_shares SET rel_path=NULL, blob=NULL WHERE token=?1",
                    [&token]
                )
                .is_err()
            );
            assert!(
                db.execute(
                    "UPDATE file_shares SET rel_path='a', blob='{}' WHERE token=?1",
                    [&token]
                )
                .is_err()
            );
        }
    }

    #[test]
    fn legacy_migration_preserves_every_field_and_refuses_unmapped_identity() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE file_shares (
            token TEXT PRIMARY KEY, account_id INTEGER NOT NULL, rel_path TEXT NOT NULL,
            kind TEXT NOT NULL, password_hash TEXT, expires_at INTEGER, created_at INTEGER NOT NULL,
            revoked INTEGER NOT NULL DEFAULT 0, download_count INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX idx_file_shares_account ON file_shares(account_id) WHERE revoked=0;",
        )
        .unwrap();
        let token = mint_token();
        db.execute(
            "INSERT INTO file_shares VALUES (?1,7,'Docs/a','file','bcrypt',50,1,0,12)",
            [&token],
        )
        .unwrap();
        init_schema(&db).unwrap();
        init_schema(&db).unwrap();
        let row = db
            .query_row("SELECT * FROM file_shares", [], Row::read)
            .unwrap();
        assert_eq!(row.account, None);
        assert_eq!(row.maild_account_id, Some(7));
        assert_eq!(row.rel_path.as_deref(), Some("Docs/a"));
        assert_eq!(row.password_hash.as_deref(), Some("bcrypt"));
        assert_eq!(
            (
                row.expires_at,
                row.created_at,
                row.revoked,
                row.download_count
            ),
            (Some(50), 1, false, 12)
        );
        assert!(matches!(resolve(&db, "a.example",  &token, 2), Err(Error::NotFound)));
        db.execute(
            "UPDATE file_shares SET account=?1, primary_fqdn='a.example' WHERE token=?2",
            params![ACCOUNT, token],
        )
        .unwrap();
        assert_eq!(resolve(&db, "a.example",  &token, 2).unwrap().share.account, ACCOUNT);
    }

    #[test]
    fn migration_failure_rolls_back_original_table() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE file_shares (token TEXT, account_id INTEGER);
            INSERT INTO file_shares VALUES ('untouched',7);",
        )
        .unwrap();
        assert!(init_schema(&db).is_err());
        assert_eq!(
            db.query_row("SELECT token FROM file_shares", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "untouched"
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='file_shares_p5'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn account_scope_gates_and_telemetry() {
        let db = db();
        let token = create(&db, "a.example",  ACCOUNT, "file", &path(), Some("hash"), Some(10), 1).unwrap();
        assert!(
            list(&db, "a.example",  "other@example.test", None, 100)
                .unwrap()
                .is_empty()
        );
        assert!(!revoke(&db, "a.example",  "other@example.test", &token).unwrap());
        let gate = resolve(&db, "a.example",  &token, 2).unwrap();
        assert!(matches!(gate.authorize(None), Err(Error::Unauthorized)));
        assert!(gate.authorize(Some("hash")).is_ok());
        assert!(matches!(resolve(&db, "a.example",  &token, 10), Err(Error::Expired)));
        bump_download(&db, &token).unwrap();
        assert_eq!(list(&db, "a.example",  ACCOUNT, None, 100).unwrap()[0].download_count, 1);
        assert!(revoke(&db, "a.example",  ACCOUNT, &token).unwrap());
        assert!(matches!(resolve(&db, "a.example",  &token, 2), Err(Error::Revoked)));
    }

    #[test]
    fn unsupported_or_corrupt_targets_fail_closed() {
        let db = db();
        for kind in ["dir", "drop", "garbage"] {
            assert!(create(&db, "a.example",  ACCOUNT, kind, &path(), None, None, 1).is_err());
        }
        for rel_path in ["", "/etc/passwd", "../x", "a/../b", "a/./b", "a//b", "a\\b"] {
            assert!(!valid_relative_path(rel_path));
        }
        let token = create(&db, "a.example",  ACCOUNT, "file", &blob(), None, None, 1).unwrap();
        db.execute(
            "UPDATE file_shares SET kind='unknown' WHERE token=?1",
            [&token],
        )
        .unwrap();
        assert!(matches!(resolve(&db, "a.example",  &token, 2), Err(Error::NotFound)));
        db.execute(
            "UPDATE file_shares SET kind='file',blob='{}' WHERE token=?1",
            [&token],
        )
        .unwrap();
        assert!(matches!(resolve(&db, "a.example",  &token, 2), Err(Error::NotFound)));
    }

    #[test]
    fn public_roots_exclude_equal_nested_and_symlinked_paths() {
        let dir = tempfile::tempdir().unwrap();
        let public = dir.path().join("www");
        std::fs::create_dir_all(public.join("private")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&public, &link).unwrap();
        for path in [public.clone(), public.join("private"), link.join("private")] {
            let cfg = cosmix_config::node::WebdSharesConfig { roots: BTreeMap::from([(ACCOUNT.into(), path)]) };
            let mut roots = Roots::from_config(&cfg);
            roots.exclude_public(&[link.canonicalize().unwrap()]);
            assert!(roots.get(ACCOUNT).is_none());
        }
    }

    #[test]
    fn roots_skip_bad_entries_and_preserve_exact_identity() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cosmix_config::node::WebdSharesConfig {
            roots: BTreeMap::from([
                (ACCOUNT.into(), dir.path().into()),
                ("bad@example.test".into(), PathBuf::from("relative")),
                ("parent@example.test".into(), dir.path().join("../x")),
                ("missing@example.test".into(), dir.path().join("absent")),
                ("not-email".into(), dir.path().into()),
            ]),
        };
        let roots = Roots::from_config(&cfg);
        assert!(roots.get(ACCOUNT).is_some());
        for account in [
            "bad@example.test",
            "parent@example.test",
            "missing@example.test",
            "not-email",
            "USER@example.test",
        ] {
            assert!(roots.get(account).is_none());
        }
    }
}
