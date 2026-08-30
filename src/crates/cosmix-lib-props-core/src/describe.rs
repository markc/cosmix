//! Property schema entries per SPEC 07 §2.4.

use crate::path::PropPath;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PropType {
    Null,
    Bool,
    Number,
    String,
    List,
    Object,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropDescribe {
    pub path: PropPath,

    #[serde(rename = "type")]
    pub ty: PropType,

    pub mutable: bool,

    pub sensitive: bool,

    pub description: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,

    /// For `Object`: direct child paths (full dotted form). SPEC 07 §2.4.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<PropPath>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "enum")]
    pub enum_values: Option<Vec<String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,

    #[serde(skip_serializing_if = "is_false")]
    #[serde(default)]
    pub deprecated: bool,

    /// Per §3.2: paths marked transient opt out of `props.changed`.
    #[serde(skip_serializing_if = "is_false")]
    #[serde(default)]
    pub transient: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl PropDescribe {
    /// Builder for the common case: a leaf value.
    pub fn leaf(path: PropPath, ty: PropType, description: impl Into<String>) -> Self {
        Self {
            path,
            ty,
            mutable: false,
            sensitive: false,
            description: description.into(),
            format: None,
            children: None,
            enum_values: None,
            min: None,
            max: None,
            default: None,
            since: None,
            deprecated: false,
            transient: false,
        }
    }

    pub fn with_sensitive(mut self, s: bool) -> Self {
        self.sensitive = s;
        self
    }
    pub fn with_mutable(mut self, m: bool) -> Self {
        self.mutable = m;
        self
    }
    pub fn with_transient(mut self, t: bool) -> Self {
        self.transient = t;
        self
    }
    pub fn with_format(mut self, f: impl Into<String>) -> Self {
        self.format = Some(f.into());
        self
    }

    /// Format-as-unit convention: for a numeric leaf, `format` names the
    /// unit token the value is expressed in (e.g. `"dBFS"`, `"cents"`,
    /// `"seconds"`). This is a thin, documented alias over [`with_format`]
    /// so a schema author signals "this number is in unit X" uniformly;
    /// consumers read `format` as the unit for numeric types.
    pub fn with_unit(self, unit: impl Into<String>) -> Self {
        self.with_format(unit)
    }

    /// Inclusive lower bound for a numeric leaf. SPEC 07 §2.4 `min`.
    pub fn with_min(mut self, min: f64) -> Self {
        self.min = Some(min);
        self
    }

    /// Inclusive upper bound for a numeric leaf. SPEC 07 §2.4 `max`.
    pub fn with_max(mut self, max: f64) -> Self {
        self.max = Some(max);
        self
    }

    /// Default value advertised for the leaf. SPEC 07 §2.4 `default`.
    pub fn with_default(mut self, v: impl Into<serde_json::Value>) -> Self {
        self.default = Some(v.into());
        self
    }

    /// Permitted values for an enum-typed leaf. SPEC 07 §2.4 `enum`.
    pub fn with_enum<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.enum_values = Some(values.into_iter().map(Into::into).collect());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_serializes_minimally() {
        let d = PropDescribe::leaf(
            PropPath::new("config.bind").unwrap(),
            PropType::String,
            "WireGuard interface address",
        )
        .with_format("host:port");
        let j = serde_json::to_value(&d).unwrap();
        assert_eq!(j["path"], "config.bind");
        assert_eq!(j["type"], "string");
        assert_eq!(j["mutable"], false);
        assert_eq!(j["sensitive"], false);
        assert_eq!(j["format"], "host:port");
        // optionals absent
        assert!(j.get("default").is_none());
        assert!(j.get("transient").is_none());
        assert!(j.get("deprecated").is_none());
    }

    #[test]
    fn numeric_builders_and_unit_convention() {
        let d = PropDescribe::leaf(
            PropPath::new("mixer.channels.0.fader").unwrap(),
            PropType::Number,
            "channel fader",
        )
        .with_mutable(true)
        .with_min(-96.0)
        .with_max(12.0)
        .with_default(0.0)
        .with_unit("dBFS");
        let j = serde_json::to_value(&d).unwrap();
        assert_eq!(j["type"], "number");
        assert_eq!(j["mutable"], true);
        assert_eq!(j["min"], -96.0);
        assert_eq!(j["max"], 12.0);
        assert_eq!(j["default"], 0.0);
        // format-as-unit convention: `format` carries the unit token.
        assert_eq!(j["format"], "dBFS");
    }

    #[test]
    fn enum_builder() {
        let d = PropDescribe::leaf(
            PropPath::new("transport.state").unwrap(),
            PropType::String,
            "transport state",
        )
        .with_enum(["stopped", "playing", "paused"]);
        let j = serde_json::to_value(&d).unwrap();
        let vals: Vec<&str> = j["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(vals, ["stopped", "playing", "paused"]);
    }
}
