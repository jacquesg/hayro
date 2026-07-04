//! Strings.

use crate::crypto::DecryptionTarget;
use crate::filter::ascii_hex;
use crate::object::Object;
use crate::object::macros::object;
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::trivia::is_white_space_character;
use core::borrow::Borrow;
use core::hash::{Hash, Hasher};
use core::ops::Deref;
use smallvec::SmallVec;

/// Lexical form of a PDF [`String`] as it appeared in source.
///
/// Distinguishes the literal `(…)` and hexadecimal `<…>` forms as they
/// appear in source. A consumer that cares about source-level
/// constraints — diagnostics, byte-length limits before decoding, or
/// round-trip rewriting — uses this to discriminate between them.
///
/// ISO 32000-1 §7.3.4 defines exactly two string forms (literal and
/// hexadecimal), so this enum has a fixed cardinality and is deliberately
/// NOT `#[non_exhaustive]`: it can never gain a variant, and downstream
/// code may match it exhaustively.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StringKind {
    /// `(…)` literal string.
    Literal,
    /// `<…>` hexadecimal string.
    Hex,
}

#[derive(Clone)]
enum StringInner<'a> {
    /// A literal `(…)` string whose source needs no escape processing:
    /// the decoded value is byte-identical to `source` and borrowed
    /// zero-copy.
    LiteralBorrowed { source: &'a [u8] },
    /// A literal `(…)` string containing escape sequences. `source` is
    /// the raw pre-decode bytes; `decoded` is the escape-expanded value,
    /// held in a `SmallVec` so short strings stay inline (no heap
    /// allocation).
    LiteralOwned {
        source: &'a [u8],
        decoded: SmallVec<[u8; 23]>,
    },
    /// A hex `<…>` string. `source` is the bytes between the angle
    /// brackets (whitespace preserved) — i.e. the ASCII hex encoding;
    /// `decoded` is the decoded bytes, kept inline for short strings.
    Hex {
        source: &'a [u8],
        decoded: SmallVec<[u8; 23]>,
    },
    /// Produced by decryption of a literal or hex ciphertext. `source`
    /// is the raw bytes between the `(…)` or `<…>` delimiters exactly as
    /// they appeared in the PDF, BEFORE decryption. For a hex container
    /// this is therefore the ASCII hex ENCODING of the ciphertext, not
    /// the raw ciphertext bytes themselves; for a literal container it is
    /// the escape-uninterpreted bytes. `kind` is the lexical form of that
    /// container. `decoded` is the post-decryption plaintext.
    Decrypted {
        source: &'a [u8],
        kind: StringKind,
        decoded: SmallVec<[u8; 23]>,
    },
}

impl<'a> StringInner<'a> {
    fn decoded(&self) -> &[u8] {
        match self {
            Self::LiteralBorrowed { source } => source,
            Self::LiteralOwned { decoded, .. }
            | Self::Hex { decoded, .. }
            | Self::Decrypted { decoded, .. } => decoded,
        }
    }

    fn source(&self) -> &'a [u8] {
        match *self {
            Self::LiteralBorrowed { source }
            | Self::LiteralOwned { source, .. }
            | Self::Hex { source, .. }
            | Self::Decrypted { source, .. } => source,
        }
    }

    fn kind(&self) -> StringKind {
        match self {
            Self::LiteralBorrowed { .. } | Self::LiteralOwned { .. } => StringKind::Literal,
            Self::Hex { .. } => StringKind::Hex,
            Self::Decrypted { kind, .. } => *kind,
        }
    }
}

/// A PDF string object.
#[derive(Clone)]
pub struct String<'a>(StringInner<'a>);

impl<'a> String<'a> {
    /// Returns the string data as a byte slice.
    pub fn as_bytes(&self) -> &[u8] {
        self.as_ref()
    }

    /// The lexical kind of this string as it appeared in source.
    ///
    /// For strings whose [`as_bytes`](Self::as_bytes) value is the result
    /// of decryption, this reports the lexical form of the ciphertext
    /// container (the `(…)` or `<…>` wrapper around the encrypted
    /// bytes).
    pub fn kind(&self) -> StringKind {
        self.0.kind()
    }

    /// The raw source bytes of this string, EXCLUDING the delimiters.
    ///
    /// For a literal string `(Hello\n)` this returns `b"Hello\\n"`
    /// — the backslash-escape is left uninterpreted. For a hex string
    /// `<48656C 6C6F>` this returns `b"48656C 6C6F"` — the ASCII hex
    /// encoding, whitespace preserved. For a decrypted string this
    /// returns the raw bytes exactly as they appeared between the
    /// delimiters in the PDF source, BEFORE decryption — for a hex
    /// container that is the ASCII hex encoding of the ciphertext (not
    /// the raw ciphertext bytes), and for a literal container the
    /// escape-uninterpreted bytes. Call [`as_bytes`](Self::as_bytes)
    /// for the decrypted plaintext.
    pub fn source(&self) -> &'a [u8] {
        self.0.source()
    }
}

impl Deref for String<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl AsRef<[u8]> for String<'_> {
    fn as_ref(&self) -> &[u8] {
        self.0.decoded()
    }
}

impl Borrow<[u8]> for String<'_> {
    fn borrow(&self) -> &[u8] {
        self.as_ref()
    }
}

impl PartialEq for String<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl Eq for String<'_> {}

impl Hash for String<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

impl core::fmt::Debug for String<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        <[u8] as core::fmt::Debug>::fmt(self.as_ref(), f)
    }
}

object!(String<'a>, String);

impl Skippable for String<'_> {
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        match r.peek_byte()? {
            b'<' => skip_hex(r),
            b'(' => skip_literal(r),
            _ => None,
        }
    }
}

impl<'a> Readable<'a> for String<'a> {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let inner = match r.peek_byte()? {
            b'<' => read_hex(r)?,
            b'(' => read_literal(r)?,
            _ => return None,
        };

        // Apply decryption if needed.
        let final_inner = if ctx.xref().needs_decryption(ctx) {
            if let Some(obj_number) = ctx.obj_number() {
                match ctx
                    .xref()
                    .decrypt(obj_number, inner.decoded(), DecryptionTarget::String)
                {
                    Some(plaintext) => StringInner::Decrypted {
                        source: inner.source(),
                        kind: inner.kind(),
                        decoded: SmallVec::from_vec(plaintext),
                    },
                    None => inner,
                }
            } else {
                inner
            }
        } else {
            inner
        };

        Some(Self(final_inner))
    }
}

fn skip_hex(r: &mut Reader<'_>) -> Option<()> {
    r.forward_tag(b"<")?;
    while let Some(b) = r.peek_byte() {
        let is_hex = b.is_ascii_hexdigit();
        let is_whitespace = is_white_space_character(b);

        if !is_hex && !is_whitespace {
            break;
        }

        r.read_byte()?;
    }
    r.forward_tag(b">")?;

    Some(())
}

fn read_hex<'a>(r: &mut Reader<'a>) -> Option<StringInner<'a>> {
    let start = r.offset();
    skip_hex(r)?;
    let end = r.offset();

    // Exclude outer brackets.
    let source = r.range(start + 1..end - 1)?;
    // Keep the decoded bytes in a `SmallVec` so short hex strings stay
    // inline; `into_vec()` would force a heap allocation even for a
    // handful of bytes.
    let decoded: SmallVec<[u8; 23]> = ascii_hex::decode_into(source)?;

    Some(StringInner::Hex { source, decoded })
}

fn skip_literal(r: &mut Reader<'_>) -> Option<()> {
    r.forward_tag(b"(")?;
    let mut bracket_counter = 1;

    while bracket_counter > 0 {
        let byte = r.read_byte()?;

        match byte {
            b'\\' => {
                let _ = r.read_byte()?;
            }
            b'(' => bracket_counter += 1,
            b')' => bracket_counter -= 1,
            _ => {}
        };
    }

    Some(())
}

fn read_literal<'a>(r: &mut Reader<'a>) -> Option<StringInner<'a>> {
    let start = r.offset();
    skip_literal(r)?;
    let end = r.offset();

    // Exclude outer parentheses.
    let source = r.range(start + 1..end - 1)?;

    if !source.iter().any(|b| matches!(b, b'\\' | b'\n' | b'\r')) {
        return Some(StringInner::LiteralBorrowed { source });
    }

    let mut r = Reader::new(source);
    let mut result: SmallVec<[u8; 23]> = SmallVec::new();

    while let Some(byte) = r.read_byte() {
        match byte {
            b'\\' => {
                let next = r.read_byte()?;

                if is_octal_digit(next) {
                    let second = r.read_byte();
                    let third = r.read_byte();

                    let bytes = match (second, third) {
                        (Some(n1), Some(n2)) => match (is_octal_digit(n1), is_octal_digit(n2)) {
                            (true, true) => [next, n1, n2],
                            (true, _) => {
                                r.jump(r.offset() - 1);
                                [b'0', next, n1]
                            }
                            _ => {
                                r.jump(r.offset() - 2);
                                [b'0', b'0', next]
                            }
                        },
                        (Some(n1), None) => {
                            if is_octal_digit(n1) {
                                [b'0', next, n1]
                            } else {
                                r.jump(r.offset() - 1);
                                [b'0', b'0', next]
                            }
                        }
                        _ => [b'0', b'0', next],
                    };

                    let str = core::str::from_utf8(&bytes).unwrap();

                    if let Ok(num) = u8::from_str_radix(str, 8) {
                        result.push(num);
                    } else {
                        warn!("overflow occurred while parsing octal literal string");
                    }
                } else {
                    match next {
                        b'n' => result.push(0xA),
                        b'r' => result.push(0xD),
                        b't' => result.push(0x9),
                        b'b' => result.push(0x8),
                        b'f' => result.push(0xC),
                        b'(' => result.push(b'('),
                        b')' => result.push(b')'),
                        b'\\' => result.push(b'\\'),
                        b'\n' | b'\r' => {
                            // A conforming reader shall disregard the REVERSE SOLIDUS
                            // and the end-of-line marker following it when reading
                            // the string; the resulting string value shall be
                            // identical to that which would be read if the string
                            // were not split.
                            r.skip_eol_characters();
                        }
                        _ => result.push(next),
                    }
                }
            }
            b'(' | b')' => result.push(byte),
            // An end-of-line marker appearing within a literal string
            // without a preceding REVERSE SOLIDUS shall be treated as
            // a byte value of (0Ah), irrespective of whether the end-of-line
            // marker was a CARRIAGE RETURN (0Dh), a LINE FEED (0Ah), or both.
            b'\n' | b'\r' => {
                result.push(b'\n');
                r.skip_eol_characters();
            }
            other => result.push(other),
        }
    }

    // Keep the escape-expanded bytes in a `SmallVec` so short literal
    // strings stay inline; `into_vec()` would force a heap allocation
    // even for a handful of bytes.
    Some(StringInner::LiteralOwned {
        source,
        decoded: result,
    })
}

fn is_octal_digit(byte: u8) -> bool {
    matches!(byte, b'0'..=b'7')
}

#[cfg(test)]
mod tests {
    use crate::object::{String, string::StringKind};
    use crate::reader::Reader;
    use crate::reader::ReaderExt;

    fn parse(bytes: &[u8]) -> String<'_> {
        Reader::new(bytes)
            .read_without_context::<String<'_>>()
            .unwrap()
    }

    #[test]
    fn hex_string_empty() {
        assert_eq!(parse(b"<>").as_bytes(), b"");
    }

    #[test]
    fn hex_string_1() {
        assert_eq!(parse(b"<00010203>").as_bytes(), &[0x00, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn hex_string_2() {
        assert_eq!(
            parse(b"<000102034>").as_bytes(),
            &[0x00, 0x01, 0x02, 0x03, 0x40]
        );
    }

    #[test]
    fn hex_string_trailing_1() {
        assert_eq!(
            parse(b"<000102034>dfgfg4").as_bytes(),
            &[0x00, 0x01, 0x02, 0x03, 0x40]
        );
    }

    #[test]
    fn hex_string_trailing_2() {
        assert_eq!(parse(b"<1  3 4>dfgfg4").as_bytes(), &[0x13, 0x40]);
    }

    #[test]
    fn hex_string_trailing_3() {
        assert_eq!(parse(b"<1>dfgfg4").as_bytes(), &[0x10]);
    }

    #[test]
    fn hex_string_invalid_1() {
        assert!(
            Reader::new(b"<")
                .read_without_context::<String<'_>>()
                .is_none()
        );
    }

    #[test]
    fn hex_string_invalid_2() {
        assert!(
            Reader::new(b"34AD")
                .read_without_context::<String<'_>>()
                .is_none()
        );
    }

    #[test]
    fn literal_string_empty() {
        assert_eq!(parse(b"()").as_bytes(), b"");
    }

    #[test]
    fn literal_string_1() {
        assert_eq!(parse(b"(Hi there.)").as_bytes(), b"Hi there.");
    }

    #[test]
    fn literal_string_2() {
        assert!(
            Reader::new(b"(Hi \\777)")
                .read_without_context::<String<'_>>()
                .is_some()
        );
    }

    #[test]
    fn literal_string_3() {
        assert_eq!(parse(b"(Hi ) there.)").as_bytes(), b"Hi ");
    }

    #[test]
    fn literal_string_4() {
        assert_eq!(parse(b"(Hi (()) there)").as_bytes(), b"Hi (()) there");
    }

    #[test]
    fn literal_string_5() {
        assert_eq!(parse(b"(Hi \\()").as_bytes(), b"Hi (");
    }

    #[test]
    fn literal_string_6() {
        assert_eq!(parse(b"(Hi \\\nthere)").as_bytes(), b"Hi there");
    }

    #[test]
    fn literal_string_7() {
        assert_eq!(parse(b"(Hi \\05354)").as_bytes(), b"Hi +54");
    }

    #[test]
    fn literal_string_8() {
        assert_eq!(parse(b"(\\3)").as_bytes(), b"\x03");
    }

    #[test]
    fn literal_string_9() {
        assert_eq!(parse(b"(\\36)").as_bytes(), b"\x1e");
    }

    #[test]
    fn literal_string_10() {
        assert_eq!(parse(b"(\\36ab)").as_bytes(), b"\x1eab");
    }

    #[test]
    fn literal_string_11() {
        assert_eq!(parse(b"(\\00Y)").as_bytes(), b"\0Y");
    }

    #[test]
    fn literal_string_12() {
        assert_eq!(parse(b"(\\0Y)").as_bytes(), b"\0Y");
    }

    #[test]
    fn literal_string_trailing() {
        assert_eq!(parse(b"(Hi there.)abcde").as_bytes(), b"Hi there.");
    }

    #[test]
    fn literal_string_invalid() {
        assert_eq!(parse(b"(Hi \\778)").as_bytes(), b"Hi \x3F8");
    }

    #[test]
    fn string_1() {
        assert_eq!(parse(b"(Hi there.)").as_bytes(), b"Hi there.");
    }

    #[test]
    fn string_2() {
        assert_eq!(parse(b"<00010203>").as_bytes(), &[0x00, 0x01, 0x02, 0x03]);
    }

    // --- PR #5: source/kind accessors ----------------------------------

    #[test]
    fn kind_and_source_for_unescaped_literal() {
        let s = parse(b"(Hello)");
        assert_eq!(s.kind(), StringKind::Literal);
        assert_eq!(s.source(), b"Hello");
        assert_eq!(s.as_bytes(), b"Hello");
    }

    #[test]
    fn source_preserves_literal_escapes_uninterpreted() {
        let s = parse(b"(He\\llo)");
        assert_eq!(s.kind(), StringKind::Literal);
        assert_eq!(s.source(), b"He\\llo");
        assert_eq!(s.as_bytes(), b"Hello");
    }

    #[test]
    fn source_preserves_line_splice_escape_uninterpreted() {
        let s = parse(b"(Hi \\\nthere)");
        assert_eq!(s.kind(), StringKind::Literal);
        assert_eq!(s.source(), b"Hi \\\nthere");
        assert_eq!(s.as_bytes(), b"Hi there");
    }

    #[test]
    fn kind_and_source_for_hex() {
        let s = parse(b"<48656C6C6F>");
        assert_eq!(s.kind(), StringKind::Hex);
        assert_eq!(s.source(), b"48656C6C6F");
        assert_eq!(s.as_bytes(), b"Hello");
    }

    #[test]
    fn odd_length_hex_source_preserved() {
        // PDF §7.3.4.3: if the final hex digit is missing, it is
        // treated as 0. `414` -> `4140` -> [0x41, 0x40].
        let s = parse(b"<414>");
        assert_eq!(s.kind(), StringKind::Hex);
        assert_eq!(s.source(), b"414");
        assert_eq!(s.as_bytes(), &[0x41, 0x40]);
    }

    #[test]
    fn hex_whitespace_preserved_in_source() {
        let s = parse(b"<48 65 6C 6C 6F>");
        assert_eq!(s.kind(), StringKind::Hex);
        assert_eq!(s.source(), b"48 65 6C 6C 6F");
        assert_eq!(s.as_bytes(), b"Hello");
    }

    #[test]
    fn source_reports_span_inside_larger_buffer() {
        // Make sure offsets are bound by the delimiters, not the buffer.
        let s = parse(b"(Hi)xyz");
        assert_eq!(s.source(), b"Hi");
    }

    #[test]
    fn decrypted_string_source_is_ciphertext() {
        use crate::Pdf;
        use crate::object::{Dict, Object};
        use alloc::vec::Vec;

        let bytes: &[u8] = include_bytes!("../../../beeld-tests/pdfs/custom/encrypted_aes_128.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("fixture loads");

        fn walk_object<'a>(obj: &Object<'a>, out: &mut Vec<String<'a>>) {
            match obj {
                Object::String(s) => out.push(s.clone()),
                Object::Array(a) => {
                    for item in a.iter::<Object<'_>>() {
                        walk_object(&item, out);
                    }
                }
                Object::Dict(d) => walk_dict(d, out),
                Object::Stream(s) => walk_dict(s.dict(), out),
                _ => {}
            }
        }

        fn walk_dict<'a>(d: &Dict<'a>, out: &mut Vec<String<'a>>) {
            let keys: Vec<Vec<u8>> = d.keys().map(|k| k.as_ref().to_vec()).collect();
            for key in keys {
                if let Some(v) = d.get::<Object<'_>>(&key) {
                    walk_object(&v, out);
                }
            }
        }

        let mut strings: Vec<String<'_>> = Vec::new();
        for object in pdf.objects() {
            walk_object(&object, &mut strings);
        }

        assert!(
            !strings.is_empty(),
            "encrypted fixture must expose at least one String object"
        );

        let mut found_ciphertext_differs = false;
        let mut found_any_non_empty = false;
        for s in &strings {
            if s.as_bytes().is_empty() {
                continue;
            }
            found_any_non_empty = true;
            if s.source() != s.as_bytes() {
                found_ciphertext_differs = true;
                break;
            }
        }
        assert!(
            found_any_non_empty,
            "no non-empty decrypted string in fixture"
        );
        assert!(
            found_ciphertext_differs,
            "expected at least one decrypted string whose ciphertext source \
             differs from its plaintext bytes"
        );
    }

    #[test]
    fn object_size_regression() {
        // Retaining source spans + lexical kind widens String<'_>. Lock the
        // new size so surprise growth is caught. The L3 SmallVec swap (Cow
        // -> inline SmallVec for the owned hex/escaped decoded bytes) must
        // not grow this past the 56-byte budget.
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<String<'_>>(), 56);
    }

    // --- L3: short hex/escaped strings stay inline (no heap alloc). The
    // inline capacity is 23 bytes; decoding must be byte-for-byte identical
    // whether the value stays inline or spills to the heap. ---

    #[test]
    fn hex_decode_identical_inline_and_spilled() {
        use alloc::vec::Vec;
        // 13 decoded bytes -> inline.
        let inline = parse(b"<00112233445566778899AABBCC>");
        assert_eq!(
            inline.as_bytes(),
            &[
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC
            ]
        );
        assert_eq!(inline.kind(), StringKind::Hex);
        // 30 decoded bytes (> 23) -> heap spill; must decode identically.
        let spilled = parse(b"<000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D>");
        let expected: Vec<u8> = (0_u8..30).collect();
        assert_eq!(spilled.as_bytes(), expected.as_slice());
        assert_eq!(spilled.kind(), StringKind::Hex);
    }

    #[test]
    fn escaped_literal_decode_identical_inline_and_spilled() {
        // Short escaped literal stays inline.
        let inline = parse(b"(a\\nb)");
        assert_eq!(inline.as_bytes(), b"a\nb");
        assert_eq!(inline.kind(), StringKind::Literal);
        // Longer escaped literal (> 23 decoded bytes) spills to the heap;
        // the escape handling and byte values must be unchanged.
        let spilled = parse(b"(abcdefghijklmnopqrstuvwxyz0123456789\\n)");
        assert_eq!(
            spilled.as_bytes(),
            b"abcdefghijklmnopqrstuvwxyz0123456789\n"
        );
        assert_eq!(spilled.kind(), StringKind::Literal);
    }
}
