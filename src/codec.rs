//! Bytes: a small, owned format for saving and sending what's built on
//! mamre.
//!
//! A type that can go to bytes implements [`Codec`]. Integers are LEB128
//! varints (signed ones zigzagged), so small numbers take a byte; runs of
//! items can be written together ([`Codec::encode_run`]), which is how text
//! goes as one UTF-8 string rather than a char at a time.
//!
//! A [`SumTree`] saves as just its items, in order: its summaries are all
//! derived from them, so loading rebuilds them. Anything built on a sum tree
//! whose items have a codec can be saved this way.
//!
//! [`crc32c`] is a checksum for records, so a write that was cut off, or
//! bytes that went bad, are caught rather than read.

use std::fmt;

use crate::{Item, SumTree};

/// Why bytes couldn't be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// They ran out before a value ended.
    UnexpectedEnd,
    /// A varint went on past 64 bits.
    Overflow,
    /// They aren't what the format says can be there.
    Invalid(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnexpectedEnd => write!(f, "the bytes end early"),
            DecodeError::Overflow => write!(f, "a number is too big"),
            DecodeError::Invalid(what) => write!(f, "invalid: {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// A value that goes to bytes and back.
pub trait Codec: Sized {
    /// Appends this value's bytes to `out`.
    fn encode(&self, out: &mut Vec<u8>);

    /// Reads a value from the front of `input`, leaving the rest.
    fn decode(input: &mut &[u8]) -> Result<Self, DecodeError>;

    /// Appends a run of values. By default one after another; a type can
    /// do better (text: one UTF-8 string).
    fn encode_run(items: &[Self], out: &mut Vec<u8>) {
        for item in items {
            item.encode(out);
        }
    }

    /// Reads a run of `n` values written by [`encode_run`](Self::encode_run).
    fn decode_run(n: usize, input: &mut &[u8]) -> Result<Vec<Self>, DecodeError> {
        // Don't trust `n` with an allocation: the bytes run out first.
        let mut out = Vec::with_capacity(n.min(input.len()));
        for _ in 0..n {
            out.push(Self::decode(input)?);
        }
        Ok(out)
    }
}

/// Appends `n` as an unsigned LEB128 varint.
pub fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// Reads an unsigned LEB128 varint.
pub fn get_varint(input: &mut &[u8]) -> Result<u64, DecodeError> {
    let mut n: u64 = 0;
    let mut shift = 0;
    loop {
        let (&byte, rest) = input.split_first().ok_or(DecodeError::UnexpectedEnd)?;
        *input = rest;
        let bits = u64::from(byte & 0x7f);
        if shift == 63 && bits > 1 || shift > 63 {
            return Err(DecodeError::Overflow);
        }
        n |= bits << shift;
        if byte & 0x80 == 0 {
            return Ok(n);
        }
        shift += 7;
    }
}

/// Appends a `usize` as a varint.
pub fn put_usize(out: &mut Vec<u8>, n: usize) {
    put_varint(out, n as u64);
}

/// Reads a `usize` varint.
pub fn get_usize(input: &mut &[u8]) -> Result<usize, DecodeError> {
    usize::try_from(get_varint(input)?).map_err(|_| DecodeError::Overflow)
}

/// Reads exactly `n` bytes.
pub fn get_bytes<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8], DecodeError> {
    if input.len() < n {
        return Err(DecodeError::UnexpectedEnd);
    }
    let (bytes, rest) = input.split_at(n);
    *input = rest;
    Ok(bytes)
}

/// Appends a string: its length in bytes, then its UTF-8.
pub fn put_str(out: &mut Vec<u8>, s: &str) {
    put_usize(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

/// Reads a string written by [`put_str`].
pub fn get_str<'a>(input: &mut &'a [u8]) -> Result<&'a str, DecodeError> {
    let n = get_usize(input)?;
    std::str::from_utf8(get_bytes(input, n)?).map_err(|_| DecodeError::Invalid("not UTF-8"))
}

macro_rules! unsigned {
    ($($t:ty),*) => {$(
        impl Codec for $t {
            fn encode(&self, out: &mut Vec<u8>) {
                put_varint(out, *self as u64);
            }
            fn decode(input: &mut &[u8]) -> Result<Self, DecodeError> {
                <$t>::try_from(get_varint(input)?).map_err(|_| DecodeError::Overflow)
            }
        }
    )*};
}
unsigned!(u16, u32, u64, usize);

macro_rules! signed {
    ($($t:ty),*) => {$(
        impl Codec for $t {
            fn encode(&self, out: &mut Vec<u8>) {
                // Zigzag: small magnitudes, either sign, stay small.
                let n = *self as i64;
                put_varint(out, ((n << 1) ^ (n >> 63)) as u64);
            }
            fn decode(input: &mut &[u8]) -> Result<Self, DecodeError> {
                let z = get_varint(input)?;
                let n = ((z >> 1) as i64) ^ -((z & 1) as i64);
                <$t>::try_from(n).map_err(|_| DecodeError::Overflow)
            }
        }
    )*};
}
signed!(i16, i32, i64, isize);

/// Bytes as themselves: a run is its bytes, with no length per item.
impl Codec for u8 {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(*self);
    }
    fn decode(input: &mut &[u8]) -> Result<Self, DecodeError> {
        Ok(get_bytes(input, 1)?[0])
    }
    fn encode_run(items: &[Self], out: &mut Vec<u8>) {
        out.extend_from_slice(items);
    }
    fn decode_run(n: usize, input: &mut &[u8]) -> Result<Vec<Self>, DecodeError> {
        Ok(get_bytes(input, n)?.to_vec())
    }
}

impl Codec for bool {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }
    fn decode(input: &mut &[u8]) -> Result<Self, DecodeError> {
        match get_bytes(input, 1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::Invalid("a bool is 0 or 1")),
        }
    }
}

/// A `char` alone is its code point; a run of them is one UTF-8 string, so
/// text costs what the text does.
impl Codec for char {
    fn encode(&self, out: &mut Vec<u8>) {
        put_varint(out, u64::from(*self as u32));
    }
    fn decode(input: &mut &[u8]) -> Result<Self, DecodeError> {
        let n = u32::try_from(get_varint(input)?).map_err(|_| DecodeError::Overflow)?;
        char::from_u32(n).ok_or(DecodeError::Invalid("not a char"))
    }
    fn encode_run(items: &[Self], out: &mut Vec<u8>) {
        let text: String = items.iter().collect();
        put_str(out, &text);
    }
    fn decode_run(n: usize, input: &mut &[u8]) -> Result<Vec<Self>, DecodeError> {
        let chars: Vec<char> = get_str(input)?.chars().collect();
        if chars.len() != n {
            return Err(DecodeError::Invalid("a run of chars is the wrong length"));
        }
        Ok(chars)
    }
}

impl Codec for String {
    fn encode(&self, out: &mut Vec<u8>) {
        put_str(out, self);
    }
    fn decode(input: &mut &[u8]) -> Result<Self, DecodeError> {
        get_str(input).map(str::to_string)
    }
}

impl<T: Item + Codec, const CAP: usize> SumTree<T, CAP> {
    /// Appends the tree's bytes: how many items, then the items as a run.
    /// Summaries aren't written; they're rebuilt on load.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let items = self.items();
        put_usize(out, items.len());
        T::encode_run(&items, out);
    }

    /// Reads a tree written by [`encode`](Self::encode), rebuilding its
    /// summaries.
    pub fn decode(input: &mut &[u8]) -> Result<Self, DecodeError> {
        let n = get_usize(input)?;
        Ok(T::decode_run(n, input)?.into_iter().collect())
    }
}

/// Appends a frame: a kind byte, the body's length, the body, and a
/// CRC-32C of all before it (4 bytes, little-endian). A frame that's cut
/// off or damaged is caught on reading, not read.
pub fn put_frame(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    let start = out.len();
    out.push(kind);
    put_usize(out, body.len());
    out.extend_from_slice(body);
    let crc = crc32c(&out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// Reads a frame [`put_frame`] wrote from the front of `input`: its kind
/// and body, if it's whole and its checksum matches.
pub fn get_frame<'a>(input: &mut &'a [u8]) -> Result<(u8, &'a [u8]), DecodeError> {
    let all = *input;
    let mut rest = all;
    let kind = get_bytes(&mut rest, 1)?[0];
    let len = get_usize(&mut rest)?;
    let body = get_bytes(&mut rest, len)?;
    let crc = u32::from_le_bytes(get_bytes(&mut rest, 4)?.try_into().expect("four bytes"));
    let total = all.len() - rest.len();
    if crc32c(&all[..total - 4]) != crc {
        return Err(DecodeError::Invalid("a frame's checksum doesn't match"));
    }
    *input = rest;
    Ok((kind, body))
}

/// The CRC-32C (Castagnoli) checksum of `bytes`.
pub fn crc32c(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0x82f6_3b78 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            table[i] = c;
            i += 1;
        }
        table
    };
    let mut crc = !0u32;
    for &b in bytes {
        crc = TABLE[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Summary;
    use proptest::prelude::*;

    /// A number as a tree item, summed.
    #[derive(Clone, Debug, PartialEq)]
    struct N(u32);

    #[derive(Clone, Debug, Default, PartialEq)]
    struct Sum(u64);

    impl Summary for Sum {
        fn empty() -> Self {
            Sum(0)
        }
        fn concat(&mut self, other: &Self) {
            self.0 += other.0;
        }
    }

    impl Item for N {
        type Summary = Sum;
        fn summary(&self) -> Sum {
            Sum(u64::from(self.0))
        }
    }

    impl Codec for N {
        fn encode(&self, out: &mut Vec<u8>) {
            self.0.encode(out);
        }
        fn decode(input: &mut &[u8]) -> Result<Self, DecodeError> {
            u32::decode(input).map(N)
        }
    }

    fn round_trip<T: Codec + PartialEq + fmt::Debug>(v: T) -> Result<(), TestCaseError> {
        let mut out = Vec::new();
        v.encode(&mut out);
        let mut input = &out[..];
        prop_assert_eq!(T::decode(&mut input).unwrap(), v);
        prop_assert!(input.is_empty());
        Ok(())
    }

    proptest! {
        #[test]
        fn integers(a in any::<u64>(), b in any::<i64>(), c in any::<u32>(), d in any::<i16>()) {
            round_trip(a)?;
            round_trip(b)?;
            round_trip(c)?;
            round_trip(d)?;
        }

        #[test]
        fn chars_and_text(c in any::<char>(), s in ".*") {
            round_trip(c)?;
            round_trip(s.clone())?;
            let chars: Vec<char> = s.chars().collect();
            let mut out = Vec::new();
            char::encode_run(&chars, &mut out);
            prop_assert_eq!(out.len(), varint_len(s.len()) + s.len(), "text costs what it is");
            let mut input = &out[..];
            prop_assert_eq!(char::decode_run(chars.len(), &mut input).unwrap(), chars);
        }

        /// Any bytes at all: decoding fails or succeeds, never panics.
        #[test]
        fn garbage_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..40)) {
            let mut input = &bytes[..];
            let _ = u64::decode(&mut input);
            let mut input = &bytes[..];
            let _ = char::decode_run(3, &mut input);
            let mut input = &bytes[..];
            let _ = String::decode(&mut input);
            let mut input = &bytes[..];
            let _ = SumTree::<N, 4>::decode(&mut input);
        }

        #[test]
        fn trees(values in prop::collection::vec(any::<u32>(), 0..300)) {
            let tree: SumTree<N, 4> = values.iter().copied().map(N).collect();
            let mut out = Vec::new();
            tree.encode(&mut out);
            let mut input = &out[..];
            let back = SumTree::<N, 4>::decode(&mut input).unwrap();
            prop_assert_eq!(back.items(), values.into_iter().map(N).collect::<Vec<_>>());
            prop_assert_eq!(back.summary().0, tree.summary().0, "summaries rebuilt");
            back.check().map_err(TestCaseError::fail)?;
        }
    }

    fn varint_len(n: usize) -> usize {
        let mut out = Vec::new();
        put_usize(&mut out, n);
        out.len()
    }

    #[test]
    fn varints_are_small_for_small_numbers() {
        for (n, len) in [
            (0u64, 1),
            (127, 1),
            (128, 2),
            (16_383, 2),
            (16_384, 3),
            (u64::MAX, 10),
        ] {
            let mut out = Vec::new();
            put_varint(&mut out, n);
            assert_eq!(out.len(), len, "{n}");
        }
    }

    #[test]
    fn crc32c_matches_the_standard() {
        // The check value from the CRC catalogue.
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn an_overlong_varint_is_refused() {
        let bytes = [0xff; 11];
        assert_eq!(get_varint(&mut &bytes[..]), Err(DecodeError::Overflow));
    }
}
