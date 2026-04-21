//! Names.

use crate::filter::ascii_hex::decode_hex_digit;
use crate::object::Object;
use crate::object::macros::object;
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, Skippable};
use crate::trivia::is_regular_character;
use core::borrow::Borrow;
use core::fmt::{self, Debug, Formatter};
use core::hash::{Hash, Hasher};
use core::ops::Deref;
use smallvec::SmallVec;

#[derive(Clone)]
enum NameInner<'a> {
    /// A name whose PDF source contains no `#` escapes. `source` IS the
    /// logical (decoded) byte sequence.
    Borrowed { source: &'a [u8] },
    /// A name whose PDF source contains one or more `#XX` escape
    /// sequences. `source` is the pre-decode bytes; `decoded` is the
    /// escape-expanded byte sequence.
    Escaped {
        source: &'a [u8],
        decoded: SmallVec<[u8; 23]>,
    },
    /// A name constructed programmatically, without a PDF source span.
    /// [`Name::source`] returns an empty slice in this case.
    Synthetic { decoded: SmallVec<[u8; 23]> },
}

/// A PDF name object.
#[derive(Clone)]
pub struct Name<'a>(NameInner<'a>);

impl<'a> Deref for Name<'a> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl AsRef<[u8]> for Name<'_> {
    fn as_ref(&self) -> &[u8] {
        match &self.0 {
            NameInner::Borrowed { source } => source,
            NameInner::Escaped { decoded, .. } => decoded,
            NameInner::Synthetic { decoded } => decoded,
        }
    }
}

impl Borrow<[u8]> for Name<'_> {
    fn borrow(&self) -> &[u8] {
        self.as_ref()
    }
}

impl PartialEq for Name<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl Eq for Name<'_> {}

impl Hash for Name<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

impl PartialOrd for Name<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Name<'_> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.as_ref().cmp(other.as_ref())
    }
}

impl<'a> Name<'a> {
    /// Create a new name from a sequence of bytes.
    #[inline]
    pub fn new(data: &'a [u8]) -> Option<Self> {
        if !data.contains(&b'#') {
            Some(Self::new_unescaped(data))
        } else {
            Self::new_escaped(data)
        }
    }

    /// Create a new name from an unescaped byte sequence.
    #[inline]
    pub fn new_unescaped(data: &'a [u8]) -> Self {
        Self(NameInner::Borrowed { source: data })
    }

    /// Create a new name from bytes that may contain escape sequences.
    ///
    /// The input `data` is the pre-decode source (without the leading
    /// `/` solidus). The returned [`Name`] retains that slice as its
    /// source span and decodes `#XX` sequences for the logical value.
    #[inline]
    pub fn new_escaped(data: &'a [u8]) -> Option<Self> {
        let mut result = SmallVec::new();
        let mut r = Reader::new(data);

        while let Some(b) = r.read_byte() {
            if b == b'#' {
                let hex = r.read_bytes(2)?;
                result.push(decode_hex_digit(hex[0])? << 4 | decode_hex_digit(hex[1])?);
            } else {
                result.push(b);
            }
        }

        Some(Self(NameInner::Escaped {
            source: data,
            decoded: result,
        }))
    }

    /// Return a string representation of the name.
    ///
    /// Returns a placeholder in case the name is not UTF-8 encoded.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_ref()).unwrap_or("{non-ascii key}")
    }

    /// The raw source bytes of this name, EXCLUDING the leading `/`.
    ///
    /// This is the pre-decode form: `#XX` hex escapes are preserved as
    /// written. Use [`AsRef::as_ref`] / [`Deref`] for the decoded bytes.
    ///
    /// For names constructed programmatically without a PDF source
    /// (see [`Name::from_synthetic`]), returns an empty slice. Every
    /// `Name` produced via the parse path has a non-empty source.
    pub fn source(&self) -> &'a [u8] {
        match &self.0 {
            NameInner::Borrowed { source } => source,
            NameInner::Escaped { source, .. } => source,
            NameInner::Synthetic { .. } => &[],
        }
    }

    /// Construct a [`Name`] from owned decoded bytes, with no PDF source
    /// span. Intended for callers who need a [`Name`] outside of the
    /// parse path — [`Name::source`] will return an empty slice.
    #[inline]
    pub fn from_synthetic(decoded: &[u8]) -> Self {
        let mut buf: SmallVec<[u8; 23]> = SmallVec::new();
        buf.extend_from_slice(decoded);
        Self(NameInner::Synthetic { decoded: buf })
    }
}

impl Debug for Name<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match core::str::from_utf8(self.as_ref()) {
            Ok(s) => <str as Debug>::fmt(s, f),
            Err(_) => <[u8] as Debug>::fmt(self.as_ref(), f),
        }
    }
}

object!(Name<'a>, Name);

impl Skippable for Name<'_> {
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        skip_name_like(r, true).map(|_| ())
    }
}

impl<'a> Readable<'a> for Name<'a> {
    fn read(r: &mut Reader<'a>, _: &ReaderContext<'a>) -> Option<Self> {
        let start = r.offset();
        skip_name_like(r, true)?;
        let end = r.offset();

        // Exclude leading solidus.
        let data = r.range(start + 1..end)?;
        Self::new(data)
    }
}

// This method is shared by `Name` and the parser for content stream operators (which behave like
// names, except that they aren't preceded by a solidus.
pub(crate) fn skip_name_like(r: &mut Reader<'_>, solidus: bool) -> Option<()> {
    // Note that we are not validating hex escape sequences here
    // (since this method can lie on the hot path), so it's possible
    // this method will yield invalid names. Validation needs to happen during actual
    // actual reading.
    if solidus {
        r.forward_tag(b"/")?;
        r.forward_while(is_regular_character);
    } else {
        r.forward_while_1(is_regular_character)?;
    }

    Some(())
}

#[cfg(test)]
mod tests {
    use crate::object::Name;
    use crate::reader::Reader;
    use crate::reader::ReaderExt;
    use alloc::vec::Vec;
    use core::ops::Deref;

    fn parse(bytes: &[u8]) -> Name<'_> {
        Reader::new(bytes)
            .read_without_context::<Name<'_>>()
            .unwrap()
    }

    #[test]
    fn name_1() {
        assert_eq!(
            Reader::new("/".as_bytes())
                .read_without_context::<Name<'_>>()
                .unwrap()
                .deref(),
            b""
        );
    }

    #[test]
    fn name_2() {
        assert!(
            Reader::new("dfg".as_bytes())
                .read_without_context::<Name<'_>>()
                .is_none()
        );
    }

    #[test]
    fn name_3() {
        assert!(
            Reader::new("/AB#FG".as_bytes())
                .read_without_context::<Name<'_>>()
                .is_none()
        );
    }

    #[test]
    fn name_4() {
        assert_eq!(parse(b"/Name1").deref(), b"Name1");
    }

    #[test]
    fn name_5() {
        assert_eq!(parse(b"/ASomewhatLongerName").deref(), b"ASomewhatLongerName");
    }

    #[test]
    fn name_6() {
        assert_eq!(
            parse(b"/A;Name_With-Various***Characters?").deref(),
            b"A;Name_With-Various***Characters?"
        );
    }

    #[test]
    fn name_7() {
        assert_eq!(parse(b"/1.2").deref(), b"1.2");
    }

    #[test]
    fn name_8() {
        assert_eq!(parse(b"/$$").deref(), b"$$");
    }

    #[test]
    fn name_9() {
        assert_eq!(parse(b"/@pattern").deref(), b"@pattern");
    }

    #[test]
    fn name_10() {
        assert_eq!(parse(b"/.notdef").deref(), b".notdef");
    }

    #[test]
    fn name_11() {
        assert_eq!(parse(b"/lime#20Green").deref(), b"lime Green");
    }

    #[test]
    fn name_12() {
        assert_eq!(
            parse(b"/paired#28#29parentheses").deref(),
            b"paired()parentheses"
        );
    }

    #[test]
    fn name_13() {
        assert_eq!(parse(b"/The_Key_of_F#23_Minor").deref(), b"The_Key_of_F#_Minor");
    }

    #[test]
    fn name_14() {
        assert_eq!(parse(b"/A#42").deref(), b"AB");
    }

    #[test]
    fn name_15() {
        assert_eq!(parse(b"/A#3b").deref(), b"A;");
    }

    #[test]
    fn name_16() {
        assert_eq!(parse(b"/A#3B").deref(), b"A;");
    }

    #[test]
    fn name_17() {
        assert_eq!(parse(b"/k1  ").deref(), b"k1");
    }

    // --- PR #6: source accessor --------------------------------------------

    #[test]
    fn source_for_unescaped_name_equals_decoded() {
        let name = parse(b"/Hello");
        assert_eq!(name.source(), b"Hello");
        assert_eq!(name.as_ref(), b"Hello");
    }

    #[test]
    fn source_preserves_lowercase_hex_escape() {
        // #6c decodes to 'l'; the pre-decode source must keep `#6c` verbatim.
        let name = parse(b"/Hel#6co");
        assert_eq!(name.source(), b"Hel#6co");
        assert_eq!(name.as_ref(), b"Hello");
    }

    #[test]
    fn source_preserves_uppercase_hex_escape() {
        let name = parse(b"/Hel#6Co");
        assert_eq!(name.source(), b"Hel#6Co");
        assert_eq!(name.as_ref(), b"Hello");
    }

    #[test]
    fn source_preserves_space_escape() {
        let name = parse(b"/lime#20Green");
        assert_eq!(name.source(), b"lime#20Green");
        assert_eq!(name.as_ref(), b"lime Green");
    }

    #[test]
    fn source_preserves_paren_escapes() {
        let name = parse(b"/paired#28#29parentheses");
        assert_eq!(name.source(), b"paired#28#29parentheses");
        assert_eq!(name.as_ref(), b"paired()parentheses");
    }

    #[test]
    fn source_preserves_hash_escape() {
        let name = parse(b"/The_Key_of_F#23_Minor");
        assert_eq!(name.source(), b"The_Key_of_F#23_Minor");
        assert_eq!(name.as_ref(), b"The_Key_of_F#_Minor");
    }

    #[test]
    fn source_for_127_byte_name_succeeds() {
        // Build `/` + 127 bytes of 'a'.
        let mut bytes: Vec<u8> = Vec::with_capacity(128);
        bytes.push(b'/');
        bytes.extend(core::iter::repeat_n(b'a', 127));
        let name = parse(&bytes);
        assert_eq!(name.source().len(), 127);
        assert_eq!(name.as_ref().len(), 127);
    }

    #[test]
    fn source_for_200_byte_unescaped_name_is_permissive() {
        // ISO 32000-1 §7.3.5 RECOMMENDS ≤ 127 encoded bytes; hayro is
        // permissive and does not enforce the limit.
        let mut bytes: Vec<u8> = Vec::with_capacity(201);
        bytes.push(b'/');
        bytes.extend(core::iter::repeat_n(b'a', 200));
        let name = parse(&bytes);
        assert_eq!(name.source().len(), 200);
    }

    #[test]
    fn new_unescaped_exposes_source() {
        let name = Name::new_unescaped(b"Hi");
        assert_eq!(name.source(), b"Hi");
        assert_eq!(name.as_ref(), b"Hi");
    }

    #[test]
    fn new_escaped_retains_pre_decode_source() {
        let name = Name::new_escaped(b"lime#20Green").unwrap();
        assert_eq!(name.source(), b"lime#20Green");
        assert_eq!(name.as_ref(), b"lime Green");
    }

    #[test]
    fn synthetic_name_has_empty_source() {
        let name = Name::from_synthetic(b"Synthetic");
        assert_eq!(name.source(), b"");
        assert_eq!(name.as_ref(), b"Synthetic");
    }

    #[test]
    fn source_bound_to_pdf_slice_not_buffer_end() {
        // Source must only cover the name token, not trailing bytes.
        let name = parse(b"/Hi there");
        assert_eq!(name.source(), b"Hi");
    }
}
