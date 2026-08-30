//! Error type for the corpus core.

use thiserror::Error;

/// Errors raised by `cosmix-lib-files`.
#[derive(Debug, Error)]
pub enum FilesError {
    /// The document has no `---`-fenced frontmatter block where one was required.
    #[error("no frontmatter block in document")]
    NoFrontmatter,
    /// The frontmatter is structurally invalid (e.g. an unterminated fence).
    #[error("malformed frontmatter: {0}")]
    Malformed(String),
    /// A single-line frontmatter value cannot contain a newline.
    #[error("frontmatter value contains a newline (single-line values only): {0:?}")]
    MultilineValue(String),
    /// Filesystem error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The canonical header reader rejected the input.
    #[error("header parse error: {0}")]
    Parse(String),
    /// An index-database (sqlite) error (only constructed with the `sqlite` feature).
    #[error("db error: {0}")]
    Db(String),
    /// A file-manager (`fsops`) authorization failure: unknown place, path escape,
    /// or a mutation on a read-only place.
    #[error("denied: {0}")]
    Denied(String),
    /// A file-manager target that does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A file-manager target that already exists (no-clobber).
    #[error("exists: {0}")]
    Exists(String),
    /// A malformed file-manager request (e.g. delete of a dir without recursive).
    #[error("bad request: {0}")]
    BadRequest(String),
}

#[cfg(feature = "sqlite")]
impl From<rusqlite::Error> for FilesError {
    fn from(e: rusqlite::Error) -> Self {
        FilesError::Db(e.to_string())
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, FilesError>;
