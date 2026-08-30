//! Serde `Deserializer` over the Mix `Value` tree.
//!
//! Behind the `serde` feature. Lets every typed config struct keep its
//! `#[derive(Deserialize)]` and serde attributes (`default`,
//! `deny_unknown_fields`, `rename`/`rename_all`, `untagged`, …) instead
//! of a hand-written `TryFrom<Value>` per struct. The companion
//! `Serializer` (write-back) lives in `serde_ser` (C2).
//!
//! `.conf.mix` parses to a dynamic `Value` tree via [`crate::parse_data`]
//! — scalars, lists, maps, `nil`. Crucially Mix has **no integer
//! variant**: every `.conf.mix` number is a `Value::Number(f64)` (the
//! same shape `toml_to_mix` already produces from `toml::Integer`). So
//! integer config fields hydrate through an exact-safe `f64 → int`
//! conversion that rejects non-integral and out-of-lossless-range values
//! rather than truncating.
//!
//! See `_doc/planned/2026-05-29-conf-mix-config-migration.md`.

use std::fmt;

use serde::de::{
    self, Deserialize, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};

use crate::value::Value;

/// Largest integer magnitude that round-trips exactly through `f64`
/// (`2^53`). Every integer in `[-2^53, 2^53]` is exactly representable;
/// `2^53 + 1` is the first that is not. Integer config fields reject any
/// `|value| > MAX_EXACT_INT` because such a value cannot be trusted to
/// equal what the author wrote.
pub(crate) const MAX_EXACT_INT: f64 = 9_007_199_254_740_992.0; // 2^53

/// Error type for the `Value` deserializer. A flat message string is
/// enough — config-load failures are reported to an operator, not
/// matched on programmatically, and serde's derive machinery already
/// composes informative messages (`unknown field`, `invalid type`,
/// `unknown variant`) via [`de::Error`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeError(String);

impl DeError {
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DeError {}

impl de::Error for DeError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        DeError(msg.to_string())
    }
}

/// Deserialize a `T` from an already-parsed `Value` tree.
///
/// Borrows the tree for the deserialize, so `T` may borrow from it
/// (`&str` fields work when the source `Value` outlives the result).
pub fn from_value<'de, T>(value: &'de Value) -> Result<T, DeError>
where
    T: Deserialize<'de>,
{
    T::deserialize(ValueDeserializer::new(value))
}

/// Parse a `.conf.mix` strict-data source and deserialize a `T` from it.
///
/// The parsed tree is local to this call, so `T` must own all its data
/// (`DeserializeOwned`). This is the entry point config loaders use:
/// read file → `from_conf_mix_str::<Config>` → typed struct.
pub fn from_conf_mix_str<T>(source: &str) -> Result<T, DeError>
where
    T: de::DeserializeOwned,
{
    let value = crate::parse_data(source).map_err(|e| DeError(e.to_string()))?;
    from_value(&value)
}

/// Deserializer wrapping a borrowed `Value` node.
pub struct ValueDeserializer<'de> {
    value: &'de Value,
}

impl<'de> ValueDeserializer<'de> {
    pub fn new(value: &'de Value) -> Self {
        Self { value }
    }

    fn type_err(&self, expected: &str) -> DeError {
        DeError(format!(
            "expected {}, found {}",
            expected,
            self.value.type_name()
        ))
    }

    fn number(&self) -> Result<f64, DeError> {
        match self.value {
            Value::Number(n) => Ok(*n),
            _ => Err(self.type_err("number")),
        }
    }

    /// Exact `f64 → i64`. Rejects non-finite, non-integral, and
    /// out-of-lossless-range values. Per-type range (`i8`/`i16`/… can't
    /// hold every `i64`) is enforced afterwards by serde's own numeric
    /// visitor, so every signed integer field funnels through this and
    /// then `visit_i64`.
    fn exact_i64(&self) -> Result<i64, DeError> {
        let n = self.number()?;
        if !n.is_finite() {
            return Err(DeError(format!("{n} is not a finite integer")));
        }
        if n.fract() != 0.0 {
            return Err(DeError(format!(
                "{n} is not an integer (a whole number is required here)"
            )));
        }
        if n.abs() > MAX_EXACT_INT {
            return Err(DeError(format!(
                "{n} exceeds the range integers can represent exactly in this format (2^53)"
            )));
        }
        Ok(n as i64)
    }

    /// Exact `f64 → u64`. As [`Self::exact_i64`] plus a non-negative
    /// guard. Per-type range (`u8`/`u16`/…) is enforced afterwards by
    /// serde's numeric visitor via `visit_u64`.
    fn exact_u64(&self) -> Result<u64, DeError> {
        let n = self.number()?;
        if !n.is_finite() {
            return Err(DeError(format!("{n} is not a finite integer")));
        }
        if n.fract() != 0.0 {
            return Err(DeError(format!(
                "{n} is not an integer (a whole number is required here)"
            )));
        }
        if n < 0.0 {
            return Err(DeError(format!(
                "{n} is negative (an unsigned integer is required here)"
            )));
        }
        if n > MAX_EXACT_INT {
            return Err(DeError(format!(
                "{n} exceeds the range integers can represent exactly in this format (2^53)"
            )));
        }
        Ok(n as u64)
    }
}

/// Funnels each signed-integer method through the exact-safe extractor
/// and serde's `visit_i64` (which range-checks for the concrete width).
macro_rules! deserialize_signed {
    ($method:ident) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
            visitor.visit_i64(self.exact_i64()?)
        }
    };
}

/// As [`deserialize_signed`] for unsigned widths via `visit_u64`.
macro_rules! deserialize_unsigned {
    ($method:ident) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
            visitor.visit_u64(self.exact_u64()?)
        }
    };
}

impl<'de> de::Deserializer<'de> for ValueDeserializer<'de> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.value {
            Value::String(s) => visitor.visit_borrowed_str(s),
            Value::Number(n) => visitor.visit_f64(*n),
            Value::Bool(b) => visitor.visit_bool(*b),
            Value::Nil => visitor.visit_unit(),
            Value::List(items) => visitor.visit_seq(SeqDeser { iter: items.iter() }),
            Value::Map(map) => visitor.visit_map(MapDeser::new(map)),
            // Functions and raw byte buffers have no strict-data
            // representation, so they can never appear in a parsed
            // `.conf.mix` tree; reject defensively if a caller hands one
            // in via `from_value`.
            Value::Function(_) | Value::Bytes(_) | Value::Buffer(_) => {
                Err(self.type_err("a data value"))
            }
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.value {
            Value::Bool(b) => visitor.visit_bool(*b),
            _ => Err(self.type_err("bool")),
        }
    }

    deserialize_signed!(deserialize_i8);
    deserialize_signed!(deserialize_i16);
    deserialize_signed!(deserialize_i32);
    deserialize_signed!(deserialize_i64);

    fn deserialize_i128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_i128(self.exact_i64()? as i128)
    }

    deserialize_unsigned!(deserialize_u8);
    deserialize_unsigned!(deserialize_u16);
    deserialize_unsigned!(deserialize_u32);
    deserialize_unsigned!(deserialize_u64);

    fn deserialize_u128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_u128(self.exact_u64()? as u128)
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_f64(self.number()?)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_f64(self.number()?)
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.value {
            Value::String(s) => {
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => visitor.visit_char(c),
                    _ => Err(DeError(format!(
                        "expected a single character, found a string of length {}",
                        s.chars().count()
                    ))),
                }
            }
            _ => Err(self.type_err("char")),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.value {
            Value::String(s) => visitor.visit_borrowed_str(s),
            _ => Err(self.type_err("string")),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.value {
            Value::Bytes(b) => visitor.visit_borrowed_bytes(b),
            _ => Err(self.type_err("bytes")),
        }
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.value {
            Value::Nil => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.value {
            Value::Nil => visitor.visit_unit(),
            _ => Err(self.type_err("nil")),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.value {
            Value::List(items) => visitor.visit_seq(SeqDeser { iter: items.iter() }),
            _ => Err(self.type_err("list")),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.value {
            Value::Map(map) => visitor.visit_map(MapDeser::new(map)),
            _ => Err(self.type_err("map")),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        match self.value {
            // The common config shape: an externally-tagged unit variant
            // encoded as a plain string. serde's borrowed-str enum access
            // resolves the (renamed) variant name and errors on a
            // non-unit variant or an unknown name — `rename` /
            // `rename_all` are honoured entirely by the derive's variant
            // matching.
            Value::String(s) => visitor.visit_enum(s.as_str().into_deserializer()),
            // A single-key map carries a variant with a payload
            // (`{ variant: payload }`). Not used by current config enums,
            // supported for completeness.
            Value::Map(map) if map.len() == 1 => {
                let (variant, value) = map.iter().next().expect("len == 1");
                visitor.visit_enum(EnumDeser { variant, value })
            }
            Value::Map(map) => Err(DeError(format!(
                "expected a single-key map for an enum variant, found a map with {} keys",
                map.len()
            ))),
            _ => Err(self.type_err("a string or single-key map (for an enum)")),
        }
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        // Consume the value but discard it (a non-`deny_unknown_fields`
        // struct skipping an unrecognised field). `deserialize_any`
        // visits the right shape; `IgnoredAny` accepts all of them.
        self.deserialize_any(visitor)
    }
}

/// `SeqAccess` over a borrowed `Value::List`.
struct SeqDeser<'de> {
    iter: std::slice::Iter<'de, Value>,
}

impl<'de> SeqAccess<'de> for SeqDeser<'de> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, DeError> {
        match self.iter.next() {
            Some(v) => seed.deserialize(ValueDeserializer::new(v)).map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

/// `MapAccess` over a borrowed `Value::Map`.
///
/// Yields **every** key the map holds. That is the load-bearing property
/// behind `deny_unknown_fields`: an unrecognised key reaches serde's
/// field visitor (rather than being silently dropped), which is what
/// turns a config typo into an error instead of a silent default.
struct MapDeser<'de> {
    iter: indexmap::map::Iter<'de, String, Value>,
    value: Option<&'de Value>,
}

impl<'de> MapDeser<'de> {
    fn new(map: &'de indexmap::IndexMap<String, Value>) -> Self {
        Self {
            iter: map.iter(),
            value: None,
        }
    }
}

impl<'de> MapAccess<'de> for MapDeser<'de> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, DeError> {
        match self.iter.next() {
            Some((k, v)) => {
                self.value = Some(v);
                seed.deserialize(KeyDeserializer { key: k }).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DeError> {
        let v = self
            .value
            .take()
            .expect("next_value_seed called before next_key_seed");
        seed.deserialize(ValueDeserializer::new(v))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

/// Deserializer for a map key. Keys are always Mix `String`s (the parser
/// stores bareword and quoted keys identically), so every requested
/// shape resolves to the borrowed key string — including
/// `deserialize_identifier`, which is how struct field names and the
/// `deny_unknown_fields` field visitor read the key.
struct KeyDeserializer<'de> {
    key: &'de str,
}

impl<'de> de::Deserializer<'de> for KeyDeserializer<'de> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_borrowed_str(self.key)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

/// `EnumAccess` for the single-key-map enum form (`{ variant: payload }`).
struct EnumDeser<'de> {
    variant: &'de str,
    value: &'de Value,
}

impl<'de> EnumAccess<'de> for EnumDeser<'de> {
    type Error = DeError;
    type Variant = VariantDeser<'de>;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), DeError> {
        let variant = seed.deserialize(self.variant.into_deserializer())?;
        Ok((variant, VariantDeser { value: self.value }))
    }
}

struct VariantDeser<'de> {
    value: &'de Value,
}

impl<'de> VariantAccess<'de> for VariantDeser<'de> {
    type Error = DeError;

    fn unit_variant(self) -> Result<(), DeError> {
        // A unit variant in the map form is encoded as `{ variant: nil }`.
        // `deserialize_unit` accepts only `Nil`, so any payload errors.
        de::Deserialize::deserialize(ValueDeserializer::new(self.value))
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, DeError> {
        seed.deserialize(ValueDeserializer::new(self.value))
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, DeError> {
        de::Deserializer::deserialize_seq(ValueDeserializer::new(self.value), visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        de::Deserializer::deserialize_map(ValueDeserializer::new(self.value), visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use serde::Deserialize;

    fn map(pairs: &[(&str, Value)]) -> Value {
        let mut m = IndexMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        Value::map(m)
    }

    // ── Requirement 1: untagged enum (string-or-array) ──────────────

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(untagged)]
    enum ListenSpec {
        One(String),
        Many(Vec<String>),
    }

    #[test]
    fn untagged_string_arm() {
        let got: ListenSpec = from_value(&Value::String("127.0.0.1:53".into())).unwrap();
        assert_eq!(got, ListenSpec::One("127.0.0.1:53".into()));
    }

    #[test]
    fn untagged_array_arm() {
        let v = Value::list(vec![
            Value::String("a:1".into()),
            Value::String("b:2".into()),
        ]);
        let got: ListenSpec = from_value(&v).unwrap();
        assert_eq!(got, ListenSpec::Many(vec!["a:1".into(), "b:2".into()]));
    }

    // ── Requirement 2: unit-variant string enums (rename/rename_all) ─

    #[derive(Debug, PartialEq, Deserialize)]
    enum AcmeProvider {
        #[serde(rename = "letsencrypt_prod")]
        Prod,
        #[serde(rename = "letsencrypt_staging")]
        Staging,
    }

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum AcmeChallenge {
        Http01,
        Dns01,
    }

    #[test]
    fn enum_rename_valid() {
        let p: AcmeProvider = from_value(&Value::String("letsencrypt_prod".into())).unwrap();
        assert_eq!(p, AcmeProvider::Prod);
        let p: AcmeProvider = from_value(&Value::String("letsencrypt_staging".into())).unwrap();
        assert_eq!(p, AcmeProvider::Staging);
    }

    #[test]
    fn enum_rename_all_valid() {
        let c: AcmeChallenge = from_value(&Value::String("http01".into())).unwrap();
        assert_eq!(c, AcmeChallenge::Http01);
        let c: AcmeChallenge = from_value(&Value::String("dns01".into())).unwrap();
        assert_eq!(c, AcmeChallenge::Dns01);
    }

    #[test]
    fn enum_unknown_variant_errors() {
        let err = from_value::<AcmeProvider>(&Value::String("letsencrypt".into())).unwrap_err();
        assert!(
            err.message().contains("unknown variant"),
            "expected unknown-variant error, got: {err}"
        );
        let err = from_value::<AcmeChallenge>(&Value::String("tls01".into())).unwrap_err();
        assert!(
            err.message().contains("unknown variant"),
            "expected unknown-variant error, got: {err}"
        );
    }

    // ── Requirement 3: deny_unknown_fields surfaces every key ───────

    #[derive(Debug, PartialEq, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PeerConfig {
        name: String,
        port: u16,
    }

    #[test]
    fn deny_unknown_fields_accepts_known() {
        let v = map(&[
            ("name", Value::String("alpha".into())),
            ("port", Value::Number(8080.0)),
        ]);
        let got: PeerConfig = from_value(&v).unwrap();
        assert_eq!(
            got,
            PeerConfig {
                name: "alpha".into(),
                port: 8080
            }
        );
    }

    #[test]
    fn deny_unknown_fields_rejects_stale_key() {
        // A stale `hub_port` (the exact PeerConfig regression named in
        // the plan) must error, not silently default.
        let v = map(&[
            ("name", Value::String("alpha".into())),
            ("port", Value::Number(8080.0)),
            ("hub_port", Value::Number(9090.0)),
        ]);
        let err = from_value::<PeerConfig>(&v).unwrap_err();
        assert!(
            err.message().contains("unknown field") && err.message().contains("hub_port"),
            "expected unknown-field error naming hub_port, got: {err}"
        );
    }

    // ── Requirement 4: exact-safe f64 → integer ─────────────────────

    #[test]
    fn integer_valid_in_range() {
        let n: u16 = from_value(&Value::Number(8080.0)).unwrap();
        assert_eq!(n, 8080);
        let n: i64 = from_value(&Value::Number(-42.0)).unwrap();
        assert_eq!(n, -42);
    }

    #[test]
    fn integer_rejects_non_integral() {
        // 3.5 as a port — must error, never truncate to 3.
        let err = from_value::<u16>(&Value::Number(3.5)).unwrap_err();
        assert!(
            err.message().contains("not an integer"),
            "expected non-integral error, got: {err}"
        );
    }

    #[test]
    fn integer_rejects_beyond_lossless_range() {
        // 2^53 + 2 — the smallest integer ABOVE 2^53 that f64 can still
        // represent exactly (the gap at 2^53 is 2). It exceeds the
        // lossless range, so the bridge rejects it rather than trusting
        // a value that may not equal what the author wrote. (2^53 + 1
        // itself is unrepresentable and rounds to 2^53, so it is not a
        // distinguishable test input.)
        let beyond = 9_007_199_254_740_994.0_f64; // 2^53 + 2
        let err = from_value::<u64>(&Value::Number(beyond)).unwrap_err();
        assert!(
            err.message().contains("exceeds the range"),
            "expected out-of-range error, got: {err}"
        );
    }

    #[test]
    fn integer_rejects_negative_for_unsigned() {
        let err = from_value::<u32>(&Value::Number(-1.0)).unwrap_err();
        assert!(
            err.message().contains("negative"),
            "expected negative-for-unsigned error, got: {err}"
        );
    }

    #[test]
    fn integer_out_of_type_range_errors() {
        // 70000 is integral and well within lossless range, but exceeds
        // u16::MAX — serde's own numeric visitor enforces the per-type
        // bound after our exact extraction.
        let err = from_value::<u16>(&Value::Number(70000.0)).unwrap_err();
        assert!(
            err.message().contains("invalid value"),
            "expected serde out-of-range error, got: {err}"
        );
    }

    #[test]
    fn boundary_2_pow_53_accepted() {
        // Exactly 2^53 is representable and integral — it must be
        // accepted (the boundary is `> 2^53`, not `>= 2^53`).
        let n: u64 = from_value(&Value::Number(MAX_EXACT_INT)).unwrap();
        assert_eq!(n, 9_007_199_254_740_992);
    }

    // ── Requirement 5: Nil → Option::None / missing → default ───────

    #[derive(Debug, PartialEq, Deserialize)]
    struct OptHolder {
        #[serde(default)]
        maybe: Option<u32>,
        #[serde(default)]
        name: String,
    }

    #[test]
    fn nil_is_none_directly() {
        let got: Option<u32> = from_value(&Value::Nil).unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn nil_field_is_none() {
        let v = map(&[("maybe", Value::Nil), ("name", Value::String("x".into()))]);
        let got: OptHolder = from_value(&v).unwrap();
        assert_eq!(
            got,
            OptHolder {
                maybe: None,
                name: "x".into()
            }
        );
    }

    #[test]
    fn missing_field_uses_default() {
        // No `maybe` key at all — `default` fills None; absent `name`
        // fills the String default.
        let v = map(&[]);
        let got: OptHolder = from_value(&v).unwrap();
        assert_eq!(
            got,
            OptHolder {
                maybe: None,
                name: String::new()
            }
        );
    }

    #[test]
    fn present_option_is_some() {
        let v = map(&[("maybe", Value::Number(7.0))]);
        let got: OptHolder = from_value(&v).unwrap();
        assert_eq!(got.maybe, Some(7));
    }

    // ── End-to-end: parse a `.conf.mix` source then deserialize ─────

    #[test]
    fn from_conf_mix_str_top_level_body() {
        // The natural top-level `.conf.mix` form: bareword keys, newline
        // separators, no enclosing braces.
        let src = "name: \"alpha\"\nport: 8080\n";
        let got: PeerConfig = from_conf_mix_str(src).unwrap();
        assert_eq!(
            got,
            PeerConfig {
                name: "alpha".into(),
                port: 8080
            }
        );
    }

    #[test]
    fn from_conf_mix_str_propagates_parse_error() {
        // A variable reference is not strict-data; the parse error must
        // surface as a DeError, not a panic.
        let err = from_conf_mix_str::<PeerConfig>("name: $x\n").unwrap_err();
        assert!(
            err.message().contains("Strict-data violation"),
            "expected strict-data parse error, got: {err}"
        );
    }
}
