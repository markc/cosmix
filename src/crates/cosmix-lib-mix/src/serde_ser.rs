//! Serde `Serializer` building a Mix `Value` tree, plus
//! [`to_conf_mix_string`] for write-back.
//!
//! Behind the `serde` feature. The inverse of [`crate::serde_de`]: a
//! typed config struct serializes to a `Value`, then the existing,
//! round-trip-tested [`Value::to_mix_data_string`] emits the strict-data
//! `.conf.mix` text. This is what `save_service`-style auto-materialise
//! paths use to write a default config back to disk.
//!
//! Integer fields are held in `Value::Number(f64)` (Mix has no integer
//! variant), so serialization applies the **same** exact-safe gate as
//! the read side — an integer whose magnitude exceeds `2^53` is rejected
//! rather than silently rounded, keeping write-back symmetric with
//! [`crate::serde_de`].
//!
//! See `_doc/planned/2026-05-29-conf-mix-config-migration.md`.

use std::fmt;

use indexmap::IndexMap;
use serde::ser::{self, Serialize};

use crate::serde_de::MAX_EXACT_INT;
use crate::value::Value;

/// Error type for `Value` serialization. Like
/// [`crate::serde_de::DeError`], a flat message string — serialization
/// failures (a non-representable integer, a non-finite float, a
/// non-string map key, or the final `to_mix_data_string` rejecting a
/// `Function`/`Bytes`/non-finite value) are operator-facing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerError(String);

impl SerError {
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SerError {}

impl ser::Error for SerError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        SerError(msg.to_string())
    }
}

/// Serialize a `T` to a `Value` tree.
pub fn to_value<T: Serialize>(value: &T) -> Result<Value, SerError> {
    value.serialize(ValueSerializer)
}

/// Serialize a `T` to a strict-data `.conf.mix` source string.
///
/// Two stages: serialize to a `Value`, then [`Value::to_mix_data_string`]
/// emits text that round-trips through [`crate::parse_data`]. A value
/// the strict-data form can't represent (non-finite number,
/// `Function`, `Bytes`) is rejected by the second stage and surfaces as
/// a [`SerError`].
pub fn to_conf_mix_string<T: Serialize>(value: &T) -> Result<String, SerError> {
    let v = to_value(value)?;
    v.to_mix_data_string().map_err(|e| SerError(e.to_string()))
}

/// Reject an integer that `f64` can't carry exactly. Symmetric with the
/// read side's `exact_i64`/`exact_u64`.
fn checked_int_i128(n: i128) -> Result<f64, SerError> {
    if n.unsigned_abs() > MAX_EXACT_INT as u128 {
        return Err(SerError(format!(
            "integer {n} exceeds the range integers can represent exactly in this format (2^53)"
        )));
    }
    Ok(n as f64)
}

fn checked_int_u128(n: u128) -> Result<f64, SerError> {
    if n > MAX_EXACT_INT as u128 {
        return Err(SerError(format!(
            "integer {n} exceeds the range integers can represent exactly in this format (2^53)"
        )));
    }
    Ok(n as f64)
}

/// Serializer producing a single `Value`.
pub struct ValueSerializer;

impl ser::Serializer for ValueSerializer {
    type Ok = Value;
    type Error = SerError;

    type SerializeSeq = SeqSerializer;
    type SerializeTuple = SeqSerializer;
    type SerializeTupleStruct = SeqSerializer;
    type SerializeTupleVariant = TupleVariantSerializer;
    type SerializeMap = MapSerializer;
    type SerializeStruct = MapSerializer;
    type SerializeStructVariant = StructVariantSerializer;

    fn serialize_bool(self, v: bool) -> Result<Value, SerError> {
        Ok(Value::Bool(v))
    }

    fn serialize_i8(self, v: i8) -> Result<Value, SerError> {
        self.serialize_i128(v as i128)
    }
    fn serialize_i16(self, v: i16) -> Result<Value, SerError> {
        self.serialize_i128(v as i128)
    }
    fn serialize_i32(self, v: i32) -> Result<Value, SerError> {
        self.serialize_i128(v as i128)
    }
    fn serialize_i64(self, v: i64) -> Result<Value, SerError> {
        self.serialize_i128(v as i128)
    }
    fn serialize_i128(self, v: i128) -> Result<Value, SerError> {
        Ok(Value::Number(checked_int_i128(v)?))
    }

    fn serialize_u8(self, v: u8) -> Result<Value, SerError> {
        self.serialize_u128(v as u128)
    }
    fn serialize_u16(self, v: u16) -> Result<Value, SerError> {
        self.serialize_u128(v as u128)
    }
    fn serialize_u32(self, v: u32) -> Result<Value, SerError> {
        self.serialize_u128(v as u128)
    }
    fn serialize_u64(self, v: u64) -> Result<Value, SerError> {
        self.serialize_u128(v as u128)
    }
    fn serialize_u128(self, v: u128) -> Result<Value, SerError> {
        Ok(Value::Number(checked_int_u128(v)?))
    }

    fn serialize_f32(self, v: f32) -> Result<Value, SerError> {
        Ok(Value::Number(v as f64))
    }
    fn serialize_f64(self, v: f64) -> Result<Value, SerError> {
        Ok(Value::Number(v))
    }

    fn serialize_char(self, v: char) -> Result<Value, SerError> {
        Ok(Value::String(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> Result<Value, SerError> {
        Ok(Value::String(v.to_string()))
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Value, SerError> {
        Ok(Value::bytes(v.to_vec()))
    }

    fn serialize_none(self) -> Result<Value, SerError> {
        Ok(Value::Nil)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<Value, SerError> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Value, SerError> {
        Ok(Value::Nil)
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<Value, SerError> {
        Ok(Value::Nil)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<Value, SerError> {
        // A unit variant serializes to the (renamed) variant name as a
        // string — the inverse of `deserialize_enum`'s string arm, so an
        // enum like `WebdAcmeProvider` round-trips.
        Ok(Value::String(variant.to_string()))
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Value, SerError> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Value, SerError> {
        let mut m = IndexMap::with_capacity(1);
        m.insert(variant.to_string(), value.serialize(ValueSerializer)?);
        Ok(Value::map(m))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<SeqSerializer, SerError> {
        Ok(SeqSerializer {
            items: Vec::with_capacity(len.unwrap_or(0)),
        })
    }
    fn serialize_tuple(self, len: usize) -> Result<SeqSerializer, SerError> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<SeqSerializer, SerError> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<TupleVariantSerializer, SerError> {
        Ok(TupleVariantSerializer {
            variant,
            items: Vec::with_capacity(len),
        })
    }

    fn serialize_map(self, len: Option<usize>) -> Result<MapSerializer, SerError> {
        Ok(MapSerializer {
            entries: IndexMap::with_capacity(len.unwrap_or(0)),
            next_key: None,
        })
    }
    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<MapSerializer, SerError> {
        self.serialize_map(Some(len))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<StructVariantSerializer, SerError> {
        Ok(StructVariantSerializer {
            variant,
            entries: IndexMap::with_capacity(len),
        })
    }
}

pub struct SeqSerializer {
    items: Vec<Value>,
}

impl ser::SerializeSeq for SeqSerializer {
    type Ok = Value;
    type Error = SerError;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), SerError> {
        self.items.push(value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, SerError> {
        Ok(Value::list(self.items))
    }
}

impl ser::SerializeTuple for SeqSerializer {
    type Ok = Value;
    type Error = SerError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), SerError> {
        ser::SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Value, SerError> {
        ser::SerializeSeq::end(self)
    }
}

impl ser::SerializeTupleStruct for SeqSerializer {
    type Ok = Value;
    type Error = SerError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), SerError> {
        ser::SerializeSeq::serialize_element(self, value)
    }
    fn end(self) -> Result<Value, SerError> {
        ser::SerializeSeq::end(self)
    }
}

pub struct TupleVariantSerializer {
    variant: &'static str,
    items: Vec<Value>,
}

impl ser::SerializeTupleVariant for TupleVariantSerializer {
    type Ok = Value;
    type Error = SerError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), SerError> {
        self.items.push(value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, SerError> {
        let mut m = IndexMap::with_capacity(1);
        m.insert(self.variant.to_string(), Value::list(self.items));
        Ok(Value::map(m))
    }
}

pub struct MapSerializer {
    entries: IndexMap<String, Value>,
    next_key: Option<String>,
}

impl ser::SerializeMap for MapSerializer {
    type Ok = Value;
    type Error = SerError;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), SerError> {
        self.next_key = Some(key.serialize(MapKeySerializer)?);
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), SerError> {
        let key = self
            .next_key
            .take()
            .ok_or_else(|| SerError("serialize_value called before serialize_key".into()))?;
        self.entries.insert(key, value.serialize(ValueSerializer)?);
        Ok(())
    }

    fn end(self) -> Result<Value, SerError> {
        Ok(Value::map(self.entries))
    }
}

impl ser::SerializeStruct for MapSerializer {
    type Ok = Value;
    type Error = SerError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), SerError> {
        self.entries
            .insert(key.to_string(), value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, SerError> {
        Ok(Value::map(self.entries))
    }
}

pub struct StructVariantSerializer {
    variant: &'static str,
    entries: IndexMap<String, Value>,
}

impl ser::SerializeStructVariant for StructVariantSerializer {
    type Ok = Value;
    type Error = SerError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), SerError> {
        self.entries
            .insert(key.to_string(), value.serialize(ValueSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, SerError> {
        let mut m = IndexMap::with_capacity(1);
        m.insert(self.variant.to_string(), Value::map(self.entries));
        Ok(Value::map(m))
    }
}

/// Serializes a map key to a `String`. `.conf.mix` maps are
/// string-keyed; an integer key is stringified (matching serde_json),
/// and any other shape is rejected rather than silently dropped.
struct MapKeySerializer;

impl ser::Serializer for MapKeySerializer {
    type Ok = String;
    type Error = SerError;

    type SerializeSeq = ser::Impossible<String, SerError>;
    type SerializeTuple = ser::Impossible<String, SerError>;
    type SerializeTupleStruct = ser::Impossible<String, SerError>;
    type SerializeTupleVariant = ser::Impossible<String, SerError>;
    type SerializeMap = ser::Impossible<String, SerError>;
    type SerializeStruct = ser::Impossible<String, SerError>;
    type SerializeStructVariant = ser::Impossible<String, SerError>;

    fn serialize_str(self, v: &str) -> Result<String, SerError> {
        Ok(v.to_string())
    }
    fn serialize_char(self, v: char) -> Result<String, SerError> {
        Ok(v.to_string())
    }
    // Stringify integer keys (HashMap<u32, _> etc.).
    fn serialize_i8(self, v: i8) -> Result<String, SerError> {
        Ok(v.to_string())
    }
    fn serialize_i16(self, v: i16) -> Result<String, SerError> {
        Ok(v.to_string())
    }
    fn serialize_i32(self, v: i32) -> Result<String, SerError> {
        Ok(v.to_string())
    }
    fn serialize_i64(self, v: i64) -> Result<String, SerError> {
        Ok(v.to_string())
    }
    fn serialize_u8(self, v: u8) -> Result<String, SerError> {
        Ok(v.to_string())
    }
    fn serialize_u16(self, v: u16) -> Result<String, SerError> {
        Ok(v.to_string())
    }
    fn serialize_u32(self, v: u32) -> Result<String, SerError> {
        Ok(v.to_string())
    }
    fn serialize_u64(self, v: u64) -> Result<String, SerError> {
        Ok(v.to_string())
    }
    // A renamed unit-variant enum used as a key resolves to its name.
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<String, SerError> {
        Ok(variant.to_string())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<String, SerError> {
        value.serialize(self)
    }

    fn serialize_bool(self, _v: bool) -> Result<String, SerError> {
        Err(key_err("bool"))
    }
    fn serialize_i128(self, _v: i128) -> Result<String, SerError> {
        Err(key_err("i128"))
    }
    fn serialize_u128(self, _v: u128) -> Result<String, SerError> {
        Err(key_err("u128"))
    }
    fn serialize_f32(self, _v: f32) -> Result<String, SerError> {
        Err(key_err("f32"))
    }
    fn serialize_f64(self, _v: f64) -> Result<String, SerError> {
        Err(key_err("f64"))
    }
    fn serialize_bytes(self, _v: &[u8]) -> Result<String, SerError> {
        Err(key_err("bytes"))
    }
    fn serialize_none(self) -> Result<String, SerError> {
        Err(key_err("none"))
    }
    fn serialize_some<T: Serialize + ?Sized>(self, _value: &T) -> Result<String, SerError> {
        Err(key_err("some"))
    }
    fn serialize_unit(self) -> Result<String, SerError> {
        Err(key_err("unit"))
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<String, SerError> {
        Err(key_err("unit struct"))
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<String, SerError> {
        Err(key_err("newtype variant"))
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, SerError> {
        Err(key_err("sequence"))
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, SerError> {
        Err(key_err("tuple"))
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, SerError> {
        Err(key_err("tuple struct"))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, SerError> {
        Err(key_err("tuple variant"))
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, SerError> {
        Err(key_err("map"))
    }
    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, SerError> {
        Err(key_err("struct"))
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, SerError> {
        Err(key_err("struct variant"))
    }
}

fn key_err(kind: &str) -> SerError {
    SerError(format!(
        "map key must be a string (or integer); found {kind}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        name: String,
        port: u16,
        ratio: f64,
        enabled: bool,
        tags: Vec<String>,
        #[serde(default)]
        note: Option<String>,
    }

    #[test]
    fn round_trip_struct() {
        let s = Sample {
            name: "alpha".into(),
            port: 8080,
            ratio: 1.5,
            enabled: true,
            tags: vec!["a".into(), "b".into()],
            note: None,
        };
        let text = to_conf_mix_string(&s).unwrap();
        let back: Sample = crate::serde_de::from_conf_mix_str(&text).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn sigil_string_round_trip() {
        // A config string containing `$`, a leading `~/`, and a newline
        // must survive `\$` / `\~` / `\n` strict-data escaping and parse
        // back byte-identical.
        let s = Sample {
            name: "~/path/$HOME\nsecond line $x".into(),
            port: 25,
            ratio: 0.0,
            enabled: false,
            tags: vec![],
            note: Some("trailing $ and ~ mid-string".into()),
        };
        let text = to_conf_mix_string(&s).unwrap();
        let back: Sample = crate::serde_de::from_conf_mix_str(&text).unwrap();
        assert_eq!(s, back);
        assert_eq!(back.name, "~/path/$HOME\nsecond line $x");
        assert_eq!(back.note.as_deref(), Some("trailing $ and ~ mid-string"));
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum AcmeProvider {
        #[serde(rename = "letsencrypt_prod")]
        Prod,
        #[serde(rename = "letsencrypt_staging")]
        Staging,
    }

    #[test]
    fn unit_variant_round_trips_as_string() {
        let v = to_value(&AcmeProvider::Prod).unwrap();
        assert_eq!(v, Value::String("letsencrypt_prod".into()));
        let back: AcmeProvider = crate::serde_de::from_value(&v).unwrap();
        assert_eq!(back, AcmeProvider::Prod);
    }

    #[test]
    fn none_serializes_to_nil() {
        let v = to_value(&Option::<u32>::None).unwrap();
        assert_eq!(v, Value::Nil);
    }

    #[test]
    fn integer_rejects_beyond_lossless_range() {
        // Write-back is symmetric with the read side: an integer past
        // 2^53 is rejected, not silently rounded.
        let big: u64 = 9_007_199_254_740_994; // 2^53 + 2
        let err = to_value(&big).unwrap_err();
        assert!(
            err.message().contains("exceeds the range"),
            "expected out-of-range error, got: {err}"
        );
    }

    #[test]
    fn integer_at_boundary_ok() {
        let edge: u64 = 9_007_199_254_740_992; // 2^53
        let v = to_value(&edge).unwrap();
        assert_eq!(v, Value::Number(MAX_EXACT_INT));
    }

    #[test]
    fn nested_map_round_trip() {
        use std::collections::BTreeMap;
        let mut m: BTreeMap<String, u32> = BTreeMap::new();
        m.insert("one".into(), 1);
        m.insert("two".into(), 2);
        let text = to_conf_mix_string(&m).unwrap();
        let back: BTreeMap<String, u32> = crate::serde_de::from_conf_mix_str(&text).unwrap();
        assert_eq!(m, back);
    }
}
