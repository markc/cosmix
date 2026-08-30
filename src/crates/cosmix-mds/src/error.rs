use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("blob not found: {0}")]
    BlobNotFound(String),

    #[error("blob corrupt: {0}")]
    BlobCorrupt(String),

    #[error("change token expired")]
    ChangeTokenExpired,

    #[error("set not found: {0}")]
    SetNotFound(String),

    /// Set with this id is already present. Previously surfaced as
    /// `Error::Other("set {uuid} already exists")` from
    /// `create_set` (cache hit + on-disk dir hit), from `import_set`
    /// (cache hit + on-disk dir hit), and from `import::finalize`
    /// (final target dir hit on `containers/<uuid>`); promoted to a
    /// typed variant so callers can match it without substring
    /// inspection (notably maild's `ensure_account_set_idempotent`
    /// which absorbs the Saga TOCTOU race between two concurrent
    /// `after_set` invocations on the same key). The `import:`
    /// prefix from the legacy `import.rs` message is dropped — the
    /// originating lifecycle path (create_set vs import_set vs
    /// finalize) is recoverable from the call-site context (the
    /// CLI's `CliError::Mds` wrapping, the test's match arm, or the
    /// backtrace).
    #[error("set {} already exists", set.0)]
    SetAlreadyExists { set: crate::SetId },

    #[error("container not found: {0}")]
    ContainerNotFound(String),

    /// Strict-create / rename collision under a parent. The previous
    /// shape was `Error::Other("container name {name:?} already exists
    /// under that parent")`; promoted to a typed variant so callers
    /// (maild's `MailStore::create_mailbox` / `rename_mailbox`) can map
    /// it to a JMAP error without substring matching the message.
    /// `parent` is `None` for root-level collisions.
    #[error(
        "container name {name:?} already exists under {}",
        parent.as_deref().unwrap_or("<root>")
    )]
    ContainerAlreadyExists {
        parent: Option<String>,
        name: String,
    },

    #[error("item not found: {0}")]
    ItemNotFound(String),

    #[error("schema migration failed: {0}")]
    SchemaMigration(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("other: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
