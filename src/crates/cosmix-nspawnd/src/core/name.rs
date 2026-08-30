use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Valid nspawn instance name. The deliberately narrow grammar is inherited
/// from the C0 driver and also makes the systemd template-unit mapping exact.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InstanceName(String);

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum NameError {
    #[error("instance name must be 2..=32 ASCII characters")]
    Length,
    #[error("instance name must start with a lowercase ASCII letter")]
    FirstCharacter,
    #[error("instance name may contain only lowercase ASCII letters, digits, and '-'")]
    Character,
}

impl InstanceName {
    pub fn parse(value: impl Into<String>) -> Result<Self, NameError> {
        let value = value.into();
        if !(2..=32).contains(&value.len()) {
            return Err(NameError::Length);
        }
        if !value.as_bytes()[0].is_ascii_lowercase() {
            return Err(NameError::FirstCharacter);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(NameError::Character);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn unit_name(&self) -> String {
        format!("systemd-nspawn@{}.service", self.0)
    }
}

impl fmt::Display for InstanceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for InstanceName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for InstanceName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_grammar_and_unit_mapping_are_exact() {
        let name = InstanceName::parse("labspoke").unwrap();
        assert_eq!(name.unit_name(), "systemd-nspawn@labspoke.service");
        for bad in ["a", "Upper", "-bad", "bad_thing", "bad/thing"] {
            assert!(InstanceName::parse(bad).is_err(), "{bad}");
        }
    }
}
