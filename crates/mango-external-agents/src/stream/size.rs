//! The compact JSON size of a value, computed without producing the JSON.
//!
//! The turn buffer budgets every queued event by its serialized length, the number a host that
//! forwards the event as JSON will actually write. [`serialized_len`] is a [`serde::Serializer`]
//! that adds up what `serde_json::to_writer` would have written: it walks the value the same way
//! and skips the work that exists only to produce bytes (escaping into a buffer, formatting
//! integers, a call per fragment). The one non-trivial piece is a string's escaped length, which
//! is the string length plus a fixed number of extra bytes per escaped character, and that sum
//! is branch-free so the compiler can vectorize it.
//!
//! The count must equal `serde_json::to_vec(value).len()` exactly: the budget is a safety limit,
//! so an undercount would admit more than the host sized its memory for. Anything this serializer
//! cannot count with certainty (a float key, a `serde_json` private marker type) is reported as
//! an error so the caller falls back to the real serializer instead of guessing.

use serde::ser::{self, Impossible, Serialize};
use std::fmt;

/// A value the counter cannot size with certainty, or that failed to serialize.
///
/// Never a verdict on the value, so it carries no detail: the caller counts the value with
/// `serde_json` instead, which decides whether it serializes at all. Zero-sized, so a `Result`
/// of it stays as cheap to return as the value itself.
#[derive(Debug)]
pub(super) struct Unsupported;

impl fmt::Display for Unsupported {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("expected a value the size counter can add up")
    }
}

impl std::error::Error for Unsupported {}

impl ser::Error for Unsupported {
    fn custom<T: fmt::Display>(_message: T) -> Self {
        Self
    }
}

/// The marker prefix `serde_json` gives the types it serializes specially (`RawValue`, an
/// arbitrary-precision `Number`). Their JSON is not the generic shape, so they are not counted here.
const SERDE_JSON_PRIVATE: &str = "$serde_json::private::";

/// How many bytes `serde_json::to_vec(value)` would produce, without producing them.
///
/// For example, `serialized_len(&"a\"b")` is 6: the string, one escape and two quotes.
///
/// # Errors
///
/// [`Unsupported`] when the value needs `serde_json` to be counted, or fails to serialize. The
/// caller counts it with `serde_json`, which returns the authoritative result.
pub(super) fn serialized_len<T: Serialize + ?Sized>(value: &T) -> Result<usize, Unsupported> {
    let mut sizer = Sizer { bytes: 0 };
    value.serialize(&mut sizer)?;
    Ok(sizer.bytes)
}

/// The extra bytes `serde_json` writes to escape `byte` inside a string: none for most, one more
/// for `"`, `\` and the five short escapes (`\b \t \n \f \r`), five more for the other control
/// characters (`\u00XX`). Non-ASCII bytes and DEL pass through unchanged.
///
/// Arithmetic rather than a `matches!`, so a loop over it compiles to vector compares.
const fn escape_extra(byte: u8) -> u8 {
    let control = (byte < 0x20) as u8;
    // 8..=13 except 11 (`\x0b`, which has no short form).
    let short = ((byte.wrapping_sub(8) < 6) as u8) & ((byte != 11) as u8);
    let marked = ((byte == b'"') as u8) | ((byte == b'\\') as u8);
    control * 5 - short * 4 + marked
}

/// `escape_extra` for every byte, for the strings too short to be worth a vector loop: a field
/// name or an id is a handful of bytes, and a table lookup beats setting the loop up.
static EXTRA: [u8; 256] = {
    let mut table = [0_u8; 256];
    let mut byte = 0;
    while byte < 256 {
        table[byte] = escape_extra(byte as u8);
        byte += 1;
    }
    table
};

fn table_extra(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .map(|&byte| usize::from(EXTRA[usize::from(byte)]))
        .sum()
}

/// How many bytes the contents of `text` take once escaped, without the surrounding quotes.
///
/// For example, a line feed is two bytes (`\n`) and U+0001 is six (`\u0001`).
fn escaped_len(text: &str) -> usize {
    // 32 bytes at a time: the widest escape adds five bytes, so a block's sum is at most 160 and
    // fits one byte, which keeps the inner loop in vector lanes.
    let (blocks, remainder) = text.as_bytes().as_chunks::<32>();
    let mut extra = 0_usize;
    for block in blocks {
        let mut sum = 0_u8;
        for &byte in block {
            sum = sum.wrapping_add(escape_extra(byte));
        }
        extra += usize::from(sum);
    }
    text.len() + extra + table_extra(remainder)
}

/// A quoted string's length.
fn quoted_len(text: &str) -> usize {
    escaped_len(text) + 2
}

/// How many decimal digits `value` takes.
fn digits(value: u64) -> usize {
    value.checked_ilog10().map_or(1, |log| log as usize + 1)
}

/// The signed form: digits plus a minus sign.
fn signed_digits(value: i64) -> usize {
    digits(value.unsigned_abs()) + usize::from(value < 0)
}

/// How many decimal digits `value` takes, for the widths `u64` does not hold.
fn digits_wide(value: u128) -> usize {
    value.checked_ilog10().map_or(1, |log| log as usize + 1)
}

/// What `serde_json` writes for a float, counted by letting `serde_json` write it.
///
/// Shortest round-trip formatting is its job, and non-finite values become `null`.
fn float_len<T: Serialize>(value: &T) -> Result<usize, Unsupported> {
    #[derive(Default)]
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count::default();
    serde_json::to_writer(&mut count, value).map_err(ser::Error::custom)?;
    Ok(count.0)
}

struct Sizer {
    bytes: usize,
}

/// A sequence, tuple or map being sized: the separator before every element but the first.
struct Compound<'a> {
    sizer: &'a mut Sizer,
    first: bool,
    /// What closes the value, after the elements: `]`, `}` or `]}` / `}}` for a variant.
    closing: usize,
}

impl<'a> Compound<'a> {
    fn open(sizer: &'a mut Sizer, opening: usize, closing: usize) -> Self {
        sizer.bytes += opening;
        Self {
            sizer,
            first: true,
            closing,
        }
    }

    fn separator(&mut self) {
        self.sizer.bytes += usize::from(!self.first);
        self.first = false;
    }

    fn element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unsupported> {
        self.separator();
        value.serialize(&mut *self.sizer)
    }

    fn finish(self) -> Result<(), Unsupported> {
        self.sizer.bytes += self.closing;
        Ok(())
    }
}

impl<'a> ser::Serializer for &'a mut Sizer {
    type Ok = ();
    type Error = Unsupported;
    type SerializeSeq = Compound<'a>;
    type SerializeTuple = Compound<'a>;
    type SerializeTupleStruct = Compound<'a>;
    type SerializeTupleVariant = Compound<'a>;
    type SerializeMap = Compound<'a>;
    type SerializeStruct = Compound<'a>;
    type SerializeStructVariant = Compound<'a>;

    fn serialize_bool(self, value: bool) -> Result<(), Unsupported> {
        self.bytes += if value { 4 } else { 5 };
        Ok(())
    }

    fn serialize_i8(self, value: i8) -> Result<(), Unsupported> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i16(self, value: i16) -> Result<(), Unsupported> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i32(self, value: i32) -> Result<(), Unsupported> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i64(self, value: i64) -> Result<(), Unsupported> {
        self.bytes += signed_digits(value);
        Ok(())
    }

    fn serialize_i128(self, value: i128) -> Result<(), Unsupported> {
        self.bytes += digits_wide(value.unsigned_abs()) + usize::from(value < 0);
        Ok(())
    }

    fn serialize_u8(self, value: u8) -> Result<(), Unsupported> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u16(self, value: u16) -> Result<(), Unsupported> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u32(self, value: u32) -> Result<(), Unsupported> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u64(self, value: u64) -> Result<(), Unsupported> {
        self.bytes += digits(value);
        Ok(())
    }

    fn serialize_u128(self, value: u128) -> Result<(), Unsupported> {
        self.bytes += digits_wide(value);
        Ok(())
    }

    fn serialize_f32(self, value: f32) -> Result<(), Unsupported> {
        self.bytes += float_len(&value)?;
        Ok(())
    }

    fn serialize_f64(self, value: f64) -> Result<(), Unsupported> {
        self.bytes += float_len(&value)?;
        Ok(())
    }

    fn serialize_char(self, value: char) -> Result<(), Unsupported> {
        let mut buffer = [0_u8; 4];
        self.serialize_str(value.encode_utf8(&mut buffer))
    }

    fn serialize_str(self, value: &str) -> Result<(), Unsupported> {
        self.bytes += quoted_len(value);
        Ok(())
    }

    /// `serde_json` writes bytes as an array of numbers.
    fn serialize_bytes(self, value: &[u8]) -> Result<(), Unsupported> {
        let mut seq = Compound::open(self, 1, 1);
        for byte in value {
            seq.element(byte)?;
        }
        seq.finish()
    }

    fn serialize_none(self) -> Result<(), Unsupported> {
        self.serialize_unit()
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), Unsupported> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Unsupported> {
        self.bytes += 4;
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Unsupported> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<(), Unsupported> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<(), Unsupported> {
        if name.starts_with(SERDE_JSON_PRIVATE) {
            return Err(Unsupported);
        }
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), Unsupported> {
        // `{"variant":value}`
        self.bytes += 1 + quoted_len(variant) + 1;
        value.serialize(&mut *self)?;
        self.bytes += 1;
        Ok(())
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Compound<'a>, Unsupported> {
        Ok(Compound::open(self, 1, 1))
    }

    fn serialize_tuple(self, len: usize) -> Result<Compound<'a>, Unsupported> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Compound<'a>, Unsupported> {
        self.serialize_seq(Some(len))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Compound<'a>, Unsupported> {
        // `{"variant":[` ... `]}`
        Ok(Compound::open(self, 1 + quoted_len(variant) + 1 + 1, 2))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Compound<'a>, Unsupported> {
        Ok(Compound::open(self, 1, 1))
    }

    fn serialize_struct(
        self,
        name: &'static str,
        _len: usize,
    ) -> Result<Compound<'a>, Unsupported> {
        if name.starts_with(SERDE_JSON_PRIVATE) {
            return Err(Unsupported);
        }
        Ok(Compound::open(self, 1, 1))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<Compound<'a>, Unsupported> {
        // `{"variant":{` ... `}}`
        Ok(Compound::open(self, 1 + quoted_len(variant) + 1 + 1, 2))
    }

    /// `serde_json` escapes each fragment the formatter writes, and escaping is per character, so
    /// the pieces add up to the escaped whole without building the string.
    fn collect_str<T: fmt::Display + ?Sized>(self, value: &T) -> Result<(), Unsupported> {
        struct Fragments<'a>(&'a mut usize);
        impl fmt::Write for Fragments<'_> {
            fn write_str(&mut self, fragment: &str) -> fmt::Result {
                *self.0 += escaped_len(fragment);
                Ok(())
            }
        }
        self.bytes += 2;
        fmt::write(&mut Fragments(&mut self.bytes), format_args!("{value}"))
            .map_err(|_| Unsupported)
    }
}

impl ser::SerializeSeq for Compound<'_> {
    type Ok = ();
    type Error = Unsupported;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unsupported> {
        self.element(value)
    }

    fn end(self) -> Result<(), Unsupported> {
        self.finish()
    }
}

impl ser::SerializeTuple for Compound<'_> {
    type Ok = ();
    type Error = Unsupported;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unsupported> {
        self.element(value)
    }

    fn end(self) -> Result<(), Unsupported> {
        self.finish()
    }
}

impl ser::SerializeTupleStruct for Compound<'_> {
    type Ok = ();
    type Error = Unsupported;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unsupported> {
        self.element(value)
    }

    fn end(self) -> Result<(), Unsupported> {
        self.finish()
    }
}

impl ser::SerializeTupleVariant for Compound<'_> {
    type Ok = ();
    type Error = Unsupported;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unsupported> {
        self.element(value)
    }

    fn end(self) -> Result<(), Unsupported> {
        self.finish()
    }
}

impl ser::SerializeMap for Compound<'_> {
    type Ok = ();
    type Error = Unsupported;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Unsupported> {
        self.separator();
        key.serialize(KeySizer(&mut *self.sizer))
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Unsupported> {
        self.sizer.bytes += 1;
        value.serialize(&mut *self.sizer)
    }

    fn end(self) -> Result<(), Unsupported> {
        self.finish()
    }
}

impl ser::SerializeStruct for Compound<'_> {
    type Ok = ();
    type Error = Unsupported;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Unsupported> {
        self.separator();
        // `"key":`
        self.sizer.bytes += quoted_len(key) + 1;
        value.serialize(&mut *self.sizer)
    }

    fn end(self) -> Result<(), Unsupported> {
        self.finish()
    }
}

impl ser::SerializeStructVariant for Compound<'_> {
    type Ok = ();
    type Error = Unsupported;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Unsupported> {
        ser::SerializeStruct::serialize_field(self, key, value)
    }

    fn end(self) -> Result<(), Unsupported> {
        self.finish()
    }
}

/// Sizes a map key: JSON object keys are strings, and `serde_json` quotes the integers, booleans
/// and characters it accepts. Any other key type is left to `serde_json`.
struct KeySizer<'a>(&'a mut Sizer);

impl KeySizer<'_> {
    fn quoted(self, unquoted: usize) -> Result<(), Unsupported> {
        self.0.bytes += unquoted + 2;
        Ok(())
    }
}

impl ser::Serializer for KeySizer<'_> {
    type Ok = ();
    type Error = Unsupported;
    type SerializeSeq = Impossible<(), Unsupported>;
    type SerializeTuple = Impossible<(), Unsupported>;
    type SerializeTupleStruct = Impossible<(), Unsupported>;
    type SerializeTupleVariant = Impossible<(), Unsupported>;
    type SerializeMap = Impossible<(), Unsupported>;
    type SerializeStruct = Impossible<(), Unsupported>;
    type SerializeStructVariant = Impossible<(), Unsupported>;

    fn serialize_bool(self, value: bool) -> Result<(), Unsupported> {
        self.quoted(if value { 4 } else { 5 })
    }

    fn serialize_i8(self, value: i8) -> Result<(), Unsupported> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i16(self, value: i16) -> Result<(), Unsupported> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i32(self, value: i32) -> Result<(), Unsupported> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i64(self, value: i64) -> Result<(), Unsupported> {
        self.quoted(signed_digits(value))
    }

    fn serialize_i128(self, value: i128) -> Result<(), Unsupported> {
        self.quoted(digits_wide(value.unsigned_abs()) + usize::from(value < 0))
    }

    fn serialize_u8(self, value: u8) -> Result<(), Unsupported> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u16(self, value: u16) -> Result<(), Unsupported> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u32(self, value: u32) -> Result<(), Unsupported> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u64(self, value: u64) -> Result<(), Unsupported> {
        self.quoted(digits(value))
    }

    fn serialize_u128(self, value: u128) -> Result<(), Unsupported> {
        self.quoted(digits_wide(value))
    }

    fn serialize_f32(self, _value: f32) -> Result<(), Unsupported> {
        Err(Unsupported)
    }

    fn serialize_f64(self, _value: f64) -> Result<(), Unsupported> {
        Err(Unsupported)
    }

    fn serialize_char(self, value: char) -> Result<(), Unsupported> {
        let mut buffer = [0_u8; 4];
        self.serialize_str(value.encode_utf8(&mut buffer))
    }

    fn serialize_str(self, value: &str) -> Result<(), Unsupported> {
        self.quoted(escaped_len(value))
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<(), Unsupported> {
        Err(Unsupported)
    }

    fn serialize_none(self) -> Result<(), Unsupported> {
        Err(Unsupported)
    }

    fn serialize_some<T: Serialize + ?Sized>(self, _value: &T) -> Result<(), Unsupported> {
        Err(Unsupported)
    }

    fn serialize_unit(self) -> Result<(), Unsupported> {
        Err(Unsupported)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Unsupported> {
        Err(Unsupported)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<(), Unsupported> {
        self.serialize_str(variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<(), Unsupported> {
        if name.starts_with(SERDE_JSON_PRIVATE) {
            return Err(Unsupported);
        }
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<(), Unsupported> {
        Err(Unsupported)
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Unsupported> {
        Err(Unsupported)
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Unsupported> {
        Err(Unsupported)
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Unsupported> {
        Err(Unsupported)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Unsupported> {
        Err(Unsupported)
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Unsupported> {
        Err(Unsupported)
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Unsupported> {
        Err(Unsupported)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Unsupported> {
        Err(Unsupported)
    }

    fn collect_str<T: fmt::Display + ?Sized>(self, value: &T) -> Result<(), Unsupported> {
        self.0.collect_str(value)
    }
}

#[cfg(test)]
pub(in crate::stream) mod tests {
    use super::{Unsupported, escaped_len, serialized_len};
    use serde::ser::{SerializeMap, SerializeSeq, SerializeStruct};
    use serde::{Serialize, Serializer};
    use std::collections::BTreeMap;
    use std::fmt;

    /// What the counter must equal: the bytes `serde_json` writes.
    fn expected_len<T: Serialize + ?Sized>(value: &T) -> usize {
        serde_json::to_vec(value)
            .expect("expected the test value to serialize with serde_json")
            .len()
    }

    /// Asserts the counter agrees with `serde_json`, naming the value on a mismatch.
    #[track_caller]
    fn assert_counted<T: Serialize + ?Sized>(label: &str, value: &T) {
        let counted = serialized_len(value).unwrap_or_else(|Unsupported| {
            panic!("expected {label} to be countable, received Unsupported")
        });
        let expected = expected_len(value);
        assert_eq!(
            counted,
            expected,
            "expected {label} to count {expected} bytes like serde_json, received {counted}: {}",
            serde_json::to_string(value).unwrap_or_default()
        );
    }

    /// A small deterministic generator, so a failing case reproduces from its seed.
    pub(in crate::stream) struct Rng(u64);

    impl Rng {
        pub(in crate::stream) fn new(seed: u64) -> Self {
            Self(seed | 1)
        }

        pub(in crate::stream) fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        pub(in crate::stream) fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    /// Pieces that cover every escape rule: the characters `serde_json` quotes, the control
    /// characters with and without a short form, DEL, and one, two, three and four byte UTF-8.
    const PIECES: [&str; 28] = [
        "a",
        "Z",
        "0",
        " ",
        "/",
        "<",
        "&",
        "'",
        "\"",
        "\\",
        "\n",
        "\r",
        "\t",
        "\u{8}",
        "\u{c}",
        "\u{0}",
        "\u{1}",
        "\u{b}",
        "\u{1f}",
        "\u{7f}",
        "\u{80}",
        "é",
        "日",
        "😀",
        "\u{2028}",
        "\u{ffff}",
        "\"\\",
        "\u{1b}[0m",
    ];

    /// Strings of every awkward length (around the 32-byte blocks) and every escape mix.
    pub(in crate::stream) fn nasty_texts() -> Vec<String> {
        let mut texts = vec![String::new()];
        // Every ASCII character alone, so each byte class is checked on its own.
        texts.extend((0_u8..128).map(|byte| char::from(byte).to_string()));
        // Plain text of lengths on both sides of the block size.
        for len in [
            1, 2, 31, 32, 33, 63, 64, 65, 95, 96, 97, 1_023, 1_024, 1_025,
        ] {
            texts.push("x".repeat(len));
            texts.push(format!("{}\"", "x".repeat(len)));
            texts.push(format!("\u{1}{}", "é".repeat(len / 2)));
        }
        // The widest escape in every position of a block: the worst case for a block's byte sum.
        for len in [31, 32, 33, 64, 1_025] {
            texts.push("\u{1}".repeat(len));
            texts.push("\"".repeat(len));
        }
        let mut rng = Rng::new(0x5EED_1234);
        for _ in 0..2_000 {
            let pieces = rng.below(120);
            let text: String = (0..pieces)
                .map(|_| PIECES[rng.below(PIECES.len())])
                .collect();
            texts.push(text);
        }
        for _ in 0..100 {
            // Mostly clean with rare escapes: the long, mostly-skipped case the block loop is for.
            let mut text = "the quick brown fox ".repeat(1 + rng.below(200));
            for _ in 0..rng.below(4) {
                let at = rng.below(text.len());
                text.insert_str(at, PIECES[rng.below(PIECES.len())]);
            }
            texts.push(text);
        }
        texts
    }

    #[test]
    fn escaped_len_equals_the_serde_json_string_length_minus_its_quotes() {
        for text in nasty_texts() {
            let expected = expected_len(&text) - 2;
            assert_eq!(
                escaped_len(&text),
                expected,
                "expected the escaped length of {text:?} to be {expected} like serde_json, received {}",
                escaped_len(&text)
            );
        }
    }

    #[test]
    fn strings_chars_and_keys_count_like_serde_json() {
        for text in nasty_texts() {
            assert_counted("a string", &text);
            assert_counted("an optional string", &Some(&text));
            assert_counted("a string in a sequence", &[&text, &text]);
            let mut map = BTreeMap::new();
            map.insert(text.clone(), text.clone());
            map.insert(format!("{text}!"), String::from("v"));
            assert_counted("a map keyed by a string", &map);
        }
        for character in ['a', '"', '\\', '\n', '\u{1}', '\u{7f}', 'é', '日', '😀'] {
            assert_counted("a char", &character);
        }
    }

    #[test]
    fn integers_booleans_unit_and_floats_count_like_serde_json() {
        assert_counted("u8 limits", &[0_u8, 9, 10, 99, 100, 255]);
        assert_counted("u16 limits", &[0_u16, 9, 10, 99, 100, 999, 1000, u16::MAX]);
        assert_counted("u32 limits", &[0_u32, 9, 10, 1_000_000, u32::MAX]);
        assert_counted(
            "u64 limits",
            &[
                0_u64,
                9,
                10,
                99,
                100,
                10_u64.pow(18),
                10_u64.pow(19),
                u64::MAX,
            ],
        );
        assert_counted(
            "i8 limits",
            &[0_i8, -1, -9, -10, -99, -100, i8::MIN, i8::MAX],
        );
        assert_counted("i32 limits", &[0_i32, -1, i32::MIN, i32::MAX]);
        assert_counted("i64 limits", &[0_i64, -1, -10, i64::MIN, i64::MAX]);
        assert_counted(
            "u128 limits",
            &[0_u128, 9, 10, u128::from(u64::MAX) + 1, u128::MAX],
        );
        assert_counted("i128 limits", &[0_i128, -1, i128::MIN, i128::MAX]);
        assert_counted("booleans", &[true, false]);
        assert_counted("a unit", &());
        assert_counted("none", &Option::<u8>::None);
        assert_counted(
            "f64 values",
            &[
                0.0_f64,
                -0.0,
                1.0,
                0.1,
                1e21,
                1e-7,
                f64::MAX,
                f64::MIN_POSITIVE,
                f64::NAN,
            ],
        );
        assert_counted("f32 values", &[0.5_f32, 1e10, f32::MAX, f32::INFINITY]);
    }

    #[derive(Serialize)]
    struct Unit;

    #[derive(Serialize)]
    struct Newtype(String);

    #[derive(Serialize)]
    struct Pair(u8, String);

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Record {
        first_field: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        skipped: Option<u32>,
        #[serde(rename = "re\"named")]
        renamed: bool,
        nested: Vec<Newtype>,
    }

    #[derive(Serialize)]
    struct Flat {
        outer: u8,
        #[serde(flatten)]
        inner: Record,
        #[serde(flatten)]
        extra: BTreeMap<String, String>,
    }

    #[derive(Serialize)]
    enum External {
        Unit,
        Newtype(String),
        Tuple(String, u64),
        Struct { text: String, count: i64 },
    }

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Internal {
        Unit,
        Struct { text: String },
        Wrapped(Record),
    }

    #[derive(Serialize)]
    #[serde(tag = "t", content = "c")]
    enum Adjacent {
        Unit,
        Newtype(String),
        Tuple(u8, u8),
        Struct { a: String },
    }

    #[derive(Serialize)]
    #[serde(untagged)]
    enum Untagged {
        Text(String),
        Number(i64),
        Pair(u8, u8),
        Record { text: String },
    }

    fn record(text: &str) -> Record {
        Record {
            first_field: text.to_owned(),
            skipped: None,
            renamed: true,
            nested: vec![Newtype(text.to_owned()), Newtype(String::new())],
        }
    }

    #[test]
    fn derived_shapes_count_like_serde_json() {
        for text in nasty_texts().iter().take(400) {
            assert_counted("a unit struct", &Unit);
            assert_counted("a newtype struct", &Newtype(text.clone()));
            assert_counted("a tuple struct", &Pair(7, text.clone()));
            assert_counted("a struct with a skipped field", &record(text));
            let mut populated = record(text);
            populated.skipped = Some(12_345);
            assert_counted("a struct with every field", &populated);
            let mut extra = BTreeMap::new();
            extra.insert(text.clone(), String::from("x"));
            assert_counted(
                "a struct with flattened fields",
                &Flat {
                    outer: 1,
                    inner: record(text),
                    extra,
                },
            );
            for (label, value) in [
                ("external unit", External::Unit),
                ("external newtype", External::Newtype(text.clone())),
                ("external tuple", External::Tuple(text.clone(), u64::MAX)),
                (
                    "external struct",
                    External::Struct {
                        text: text.clone(),
                        count: -5,
                    },
                ),
            ] {
                assert_counted(label, &value);
            }
            for (label, value) in [
                ("internal unit", Internal::Unit),
                ("internal struct", Internal::Struct { text: text.clone() }),
                (
                    "internal newtype of a struct",
                    Internal::Wrapped(record(text)),
                ),
            ] {
                assert_counted(label, &value);
            }
            for (label, value) in [
                ("adjacent unit", Adjacent::Unit),
                ("adjacent newtype", Adjacent::Newtype(text.clone())),
                ("adjacent tuple", Adjacent::Tuple(1, 22)),
                ("adjacent struct", Adjacent::Struct { a: text.clone() }),
            ] {
                assert_counted(label, &value);
            }
            for (label, value) in [
                ("untagged text", Untagged::Text(text.clone())),
                ("untagged number", Untagged::Number(-3)),
                ("untagged pair", Untagged::Pair(4, 5)),
                ("untagged record", Untagged::Record { text: text.clone() }),
            ] {
                assert_counted(label, &value);
            }
        }
    }

    #[test]
    fn maps_with_integer_boolean_and_character_keys_count_like_serde_json() {
        assert_counted(
            "an integer-keyed map",
            &BTreeMap::from([(1_u8, "a"), (200, "b")]),
        );
        assert_counted(
            "a signed-integer-keyed map",
            &BTreeMap::from([(-5_i64, 1), (i64::MIN, 2), (0, 3)]),
        );
        assert_counted("a u128-keyed map", &BTreeMap::from([(u128::MAX, 1)]));
        assert_counted("a bool-keyed map", &BTreeMap::from([(true, 1), (false, 2)]));
        assert_counted("a char-keyed map", &BTreeMap::from([('"', 1), ('é', 2)]));
        assert_counted("an empty map", &BTreeMap::<String, String>::new());
        assert_counted("an empty sequence", &Vec::<u8>::new());
        assert_counted(
            "a unit-variant-keyed map",
            &BTreeMap::from([(Key::First, 1), (Key::Second, 2)]),
        );
    }

    #[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
    enum Key {
        First,
        Second,
    }

    /// A value written as bytes, which `serde_json` spells as an array of numbers.
    struct Bytes<'a>(&'a [u8]);

    impl Serialize for Bytes<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_bytes(self.0)
        }
    }

    /// A value written through `collect_str`, as `Path`, addresses and times are.
    struct Displayed(String);

    impl fmt::Display for Displayed {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            for piece in self.0.split_inclusive('\n') {
                formatter.write_str(piece)?;
            }
            write!(formatter, "|{}|", self.0.len())
        }
    }

    impl Serialize for Displayed {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_str(self)
        }
    }

    #[test]
    fn bytes_and_displayed_values_count_like_serde_json() {
        assert_counted("empty bytes", &Bytes(&[]));
        assert_counted("one byte", &Bytes(&[7]));
        assert_counted("every byte class", &Bytes(&[0, 9, 10, 99, 100, 255, 1]));
        for text in nasty_texts().iter().take(300) {
            assert_counted("a displayed value", &Displayed(text.clone()));
            let mut map = BTreeMap::new();
            map.insert(text.as_str(), Displayed(text.clone()));
            assert_counted("a map holding a displayed value", &map);
        }
    }

    /// A map whose key is written by `collect_str`, which `serde_json` accepts as a string.
    struct DisplayedKey(String);

    impl Serialize for DisplayedKey {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry(&Displayed(self.0.clone()), &1_u8)?;
            map.end()
        }
    }

    #[test]
    fn a_key_written_by_collect_str_counts_like_serde_json() {
        for text in nasty_texts().iter().take(200) {
            assert_counted("a displayed key", &DisplayedKey(text.clone()));
        }
    }

    /// Nesting with the compound shapes written by hand: a tuple, a sequence of maps, a struct of
    /// sequences, to check the separators between and inside them.
    struct Nested(usize);

    impl Serialize for Nested {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut seq = serializer.serialize_seq(None)?;
            for index in 0..self.0 {
                seq.serialize_element(&(index as u8, "a\"b", [index, index + 1]))?;
                seq.serialize_element(&BTreeMap::from([("k", index)]))?;
            }
            seq.end()
        }
    }

    #[test]
    fn nested_compounds_count_like_serde_json() {
        for depth in 0..6 {
            assert_counted("nested compounds", &Nested(depth));
        }
        let mut record = Vec::new();
        for depth in 0..4 {
            record.push(vec![Some(Nested(depth)), None]);
        }
        assert_counted("deeply nested options", &record);
    }

    /// Values the counter must hand back to `serde_json` rather than guess at.
    struct RawLike;

    impl Serialize for RawLike {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut raw = serializer.serialize_struct("$serde_json::private::RawValue", 1)?;
            raw.serialize_field("$serde_json::private::RawValue", "{}")?;
            raw.end()
        }
    }

    #[test]
    fn values_the_counter_cannot_size_with_certainty_are_handed_to_serde_json() {
        let float_keyed = BTreeMap::from([(OrderedFloat(1.5_f64.to_bits()), 1)]);
        assert!(
            serialized_len(&float_keyed).is_err(),
            "expected a float map key to be left to serde_json, received a count"
        );
        assert!(
            serialized_len(&BTreeMap::from([(Bytes(&[1]).0.to_vec(), 1)])).is_err(),
            "expected a sequence map key to be left to serde_json, received a count"
        );
        assert!(
            serialized_len(&RawLike).is_err(),
            "expected a serde_json private marker type to be left to serde_json, received a count"
        );
    }

    /// A float keyed by its bits, so it can order a map.
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    struct OrderedFloat(u64);

    impl Serialize for OrderedFloat {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_f64(f64::from_bits(self.0))
        }
    }
}
