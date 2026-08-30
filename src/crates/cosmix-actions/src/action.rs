use std::borrow::Borrow;
use std::collections::HashSet;
use std::fmt;
use std::ops::Deref;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum UTF-8 byte length of an action id.
pub const MAX_ACTION_ID_LEN: usize = 128;

/// Maximum number of dynamically interned action ids in one process.
///
/// Static ids do not consume this budget. Re-reading an existing dynamic id is
/// free. The ceiling prevents untrusted config or future registry data from
/// turning process-lifetime interning into unbounded permanent allocation.
pub const MAX_INTERNED_ACTION_IDS: usize = 16_384;

static INTERNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();

/// A stable, process-lifetime action name.
///
/// This has the same runtime shape as CTK's original `MenuItemDef.id`:
/// a single `&'static str`.  Constants should use [`ActionId::from_static`];
/// ids read from `.mix` are deduplicated by [`ActionId::intern`] and retained
/// for the process lifetime, making the id cheap to copy through UI events.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId(&'static str);

impl ActionId {
    /// Construct an id from a program-owned string literal.
    pub const fn from_static(id: &'static str) -> Self {
        let bytes = id.as_bytes();
        assert!(!bytes.is_empty(), "action id must not be empty");
        assert!(
            bytes.len() <= MAX_ACTION_ID_LEN,
            "action id exceeds maximum length"
        );
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            assert!(
                (byte >= b'a' && byte <= b'z')
                    || (byte >= b'A' && byte <= b'Z')
                    || (byte >= b'0' && byte <= b'9')
                    || byte == b'.'
                    || byte == b'-'
                    || byte == b'_'
                    || byte == b':'
                    || byte == b'/',
                "invalid character in action id"
            );
            index += 1;
        }
        Self(id)
    }

    /// Intern a runtime string for the remainder of the process.
    pub fn intern(id: &str) -> Result<Self, ActionIdError> {
        Self::intern_many(&[id]).map(|mut ids| ids.remove(0))
    }

    /// Return the stable string representation.
    pub const fn as_str(self) -> &'static str {
        self.0
    }

    /// Validate an id without allocating or interning it.
    pub fn validate_str(id: &str) -> Result<(), ActionIdError> {
        validate(id)
    }

    pub(crate) fn intern_many(ids: &[&str]) -> Result<Vec<Self>, ActionIdError> {
        for id in ids {
            validate(id)?;
        }

        let mut interned = INTERNED
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let missing: HashSet<_> = ids
            .iter()
            .copied()
            .filter(|id| !interned.contains(*id))
            .collect();
        ensure_capacity(interned.len(), missing.len(), MAX_INTERNED_ACTION_IDS)?;

        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let stable: &'static str = if let Some(existing) = interned.get(*id) {
                existing
            } else {
                let leaked: &'static str = Box::leak((*id).to_owned().into_boxed_str());
                interned.insert(leaked);
                leaked
            };
            result.push(Self(stable));
        }
        Ok(result)
    }

    #[cfg(test)]
    pub(crate) fn is_interned(id: &str) -> bool {
        INTERNED
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(id)
    }
}

fn validate(id: &str) -> Result<(), ActionIdError> {
    if id.is_empty() {
        return Err(ActionIdError::Empty);
    }
    if id.len() > MAX_ACTION_ID_LEN {
        return Err(ActionIdError::TooLong {
            length: id.len(),
            maximum: MAX_ACTION_ID_LEN,
        });
    }
    if !id.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':' | b'/')
    }) {
        return Err(ActionIdError::InvalidCharacter);
    }
    Ok(())
}

fn ensure_capacity(current: usize, additional: usize, maximum: usize) -> Result<(), ActionIdError> {
    if current.saturating_add(additional) > maximum {
        return Err(ActionIdError::InternLimit { maximum });
    }
    Ok(())
}

/// Why a dynamic action id could not be interned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActionIdError {
    /// The id was empty.
    #[error("action id must not be empty")]
    Empty,
    /// The id contained characters outside the stable identifier vocabulary.
    #[error("action id may contain only ASCII letters, digits, '.', '-', '_', ':' and '/'")]
    InvalidCharacter,
    /// The id exceeded [`MAX_ACTION_ID_LEN`].
    #[error("action id is {length} bytes; maximum is {maximum}")]
    TooLong {
        /// Actual UTF-8 byte length.
        length: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// The process-wide dynamic interner is full.
    #[error("dynamic action-id interner limit of {maximum} reached")]
    InternLimit {
        /// Configured process-wide limit.
        maximum: usize,
    },
}

impl fmt::Debug for ActionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ActionId").field(&self.0).finish()
    }
}

impl fmt::Display for ActionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Deref for ActionId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl AsRef<str> for ActionId {
    fn as_ref(&self) -> &str {
        self.0
    }
}

impl Borrow<str> for ActionId {
    fn borrow(&self) -> &str {
        self.0
    }
}

impl From<&'static str> for ActionId {
    fn from(value: &'static str) -> Self {
        Self::from_static(value)
    }
}

impl Serialize for ActionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for ActionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let id = String::deserialize(deserializer)?;
        Self::intern(&id).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_check_rejects_before_any_allocation() {
        assert_eq!(
            ensure_capacity(MAX_INTERNED_ACTION_IDS - 1, 2, MAX_INTERNED_ACTION_IDS),
            Err(ActionIdError::InternLimit {
                maximum: MAX_INTERNED_ACTION_IDS,
            })
        );
    }
}
