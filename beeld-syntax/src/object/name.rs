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
use core::num::NonZeroU32;
use core::ops::{Deref, Range};
use smallvec::SmallVec;

/// Source offset in the parent PDF byte stream where the name token
/// BODY (the bytes after the leading `/`) starts. Stored as
/// `Option<NonZeroU32>` so the niche optimisation packs the
/// representation to 4 bytes — keeping `size_of::<Object<'_>>()` from
/// drifting when [`Name`] is embedded in the `Object` enum.
///
/// `None` for:
///   * names lexed from a DETACHED buffer — a decoded object stream, a
///     streamed-object window, or a sub-reader over a dict value / array
///     element — where the offset would not map to a file position.
///   * names constructed via [`Name::new`] / [`Name::new_unescaped`] /
///     [`Name::new_escaped`] from a free-standing byte slice — the
///     caller did not give us a parent-buffer offset to record.
///   * synthetic names (no source span at all).
///   * names whose body starts at an offset > `u32::MAX` — an offset
///     that does not fit in a `u32` (PDFs ≥ 4 GiB); see
///     [`Name::new_at`]. Beeld is permissive here:
///     `byte_range()` simply reports `None` rather than truncating
///     silently. Callers that absolutely need to byte-rewrite such
///     names in 4 GiB+ inputs must fall back to a string-search pass.
///
/// `Some(off)` only for names lexed directly from the file buffer (a
/// non-detached [`Readable<'a> for Name<'a>`] read), where `off` is the
/// absolute offset in the buffer originally passed to the `Reader`. The
/// body start is always ≥ 1 (the leading `/` lives at offset 0 at the
/// earliest), so the niche-`0` discriminant of `NonZeroU32` is available
/// for the `None` case without losing any representable offset.
type SourceOffset = Option<NonZeroU32>;

#[derive(Clone)]
enum NameInner<'a> {
    /// A name whose PDF source contains no `#` escapes. `source` IS the
    /// logical (decoded) byte sequence.
    Borrowed {
        source: &'a [u8],
        offset: SourceOffset,
    },
    /// A name whose PDF source contains one or more `#XX` escape
    /// sequences. `source` is the pre-decode bytes; `decoded` is the
    /// escape-expanded byte sequence.
    Escaped {
        source: &'a [u8],
        decoded: SmallVec<[u8; 23]>,
        offset: SourceOffset,
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
            NameInner::Borrowed { source, .. } => source,
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
        Self(NameInner::Borrowed {
            source: data,
            offset: None,
        })
    }

    /// Create a new name from bytes that may contain escape sequences.
    ///
    /// The input `data` is the pre-decode source (without the leading
    /// `/` solidus). The returned [`Name`] retains that slice as its
    /// source span and decodes `#XX` sequences for the logical value.
    #[inline]
    pub fn new_escaped(data: &'a [u8]) -> Option<Self> {
        Self::new_escaped_at(data, None)
    }

    /// Internal — same as [`Name::new`] but records the absolute source
    /// offset in the parent PDF file buffer. Used by the [`Readable`] impl
    /// so [`Name::byte_range`] returns an absolute span.
    ///
    /// `offset` is `Some(o)` where `o` is the file-absolute byte offset of
    /// the name token BODY (the position immediately after the leading
    /// `/`), or `None` when no file-absolute offset is available — the
    /// name was lexed from a detached buffer (see [`Name::byte_range`]).
    /// A `Some` offset that does not fit in a `u32` (PDFs ≥ 4 GiB) is also
    /// treated as absent; the resulting [`Name`] then carries no
    /// [`Name::byte_range`].
    #[inline]
    fn new_at(data: &'a [u8], offset: Option<usize>) -> Option<Self> {
        let packed = offset
            .and_then(|o| u32::try_from(o).ok())
            .and_then(NonZeroU32::new);
        if !data.contains(&b'#') {
            Some(Self(NameInner::Borrowed {
                source: data,
                offset: packed,
            }))
        } else {
            Self::new_escaped_at(data, packed)
        }
    }

    #[inline]
    fn new_escaped_at(data: &'a [u8], offset: SourceOffset) -> Option<Self> {
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
            offset,
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
            NameInner::Borrowed { source, .. } => source,
            NameInner::Escaped { source, .. } => source,
            NameInner::Synthetic { .. } => &[],
        }
    }

    /// Alias for [`Name::source`] — the pre-decode raw bytes of the
    /// name token (no leading `/`, `#XX` escapes preserved). Provided
    /// for symmetry with [`Name::byte_range`]: `raw_bytes()` and
    /// `byte_range()` together describe the exact slice of the parent
    /// PDF buffer this `Name` was parsed from, which downstream
    /// remediation passes need to do multi-site byte-identical
    /// rewrites of mis-encoded names.
    #[inline]
    pub fn raw_bytes(&self) -> &'a [u8] {
        self.source()
    }

    /// Absolute byte range of this name token's BODY, EXCLUDING the
    /// leading `/`, usable directly against the original PDF file buffer
    /// handed to the `Pdf::new` pipeline (or the free-standing slice a
    /// bare [`Reader`] was constructed over).
    ///
    /// This is `Some(range)` — with `range.end - range.start ==
    /// self.source().len()` — ONLY for names lexed directly from a
    /// non-detached buffer: the file buffer in the `Pdf::new` pipeline, or
    /// the free-standing slice a bare [`Reader`] was constructed over,
    /// including a top-level object's own dictionary keys.
    ///
    /// Returns `None` for:
    ///   * names lexed from a DETACHED buffer — i.e. from a decoded object
    ///     stream, a streamed-object window, or a fresh sub-reader over a
    ///     dictionary value or array element. Their offset would be
    ///     relative to an ephemeral buffer the caller cannot identify, so
    ///     `None` is reported rather than a misleading file position.
    ///   * names produced via [`Name::new`] / [`Name::new_unescaped`] /
    ///     [`Name::new_escaped`] (no parent-buffer reference is
    ///     available to the constructor).
    ///   * synthetic names (no source span at all).
    pub fn byte_range(&self) -> Option<Range<usize>> {
        let (source, offset) = match &self.0 {
            NameInner::Borrowed {
                source,
                offset: Some(off),
            } => (source, off),
            NameInner::Escaped {
                source,
                offset: Some(off),
                ..
            } => (source, off),
            NameInner::Borrowed { offset: None, .. }
            | NameInner::Escaped { offset: None, .. }
            | NameInner::Synthetic { .. } => return None,
        };
        let start = offset.get() as usize;
        Some(start..start + source.len())
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
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let start = r.offset();
        skip_name_like(r, true)?;
        let end = r.offset();

        // Exclude leading solidus.
        let body_start = start + 1;
        let data = r.range(body_start..end)?;
        // Record `body_start` as a file-absolute span ONLY when the reader
        // is over the original PDF file buffer. Under a detached context
        // (decoded object stream, streamed-object window, or a sub-reader
        // over a dict value / array element) `body_start` is relative to an
        // ephemeral buffer the caller cannot identify, so drop it — a `None`
        // `byte_range` is safer than a misleading file position.
        let offset = if ctx.detached() {
            None
        } else {
            Some(body_start)
        };
        Self::new_at(data, offset)
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
        assert_eq!(
            parse(b"/ASomewhatLongerName").deref(),
            b"ASomewhatLongerName"
        );
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
        assert_eq!(
            parse(b"/The_Key_of_F#23_Minor").deref(),
            b"The_Key_of_F#_Minor"
        );
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
        // ISO 32000-1 §7.3.5 RECOMMENDS ≤ 127 encoded bytes; beeld is
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

    // --- Phase A (Wave R-UTF8-NameToken-Substrate): byte_range / raw_bytes ---
    //
    // These accessors expose the absolute span of the name token BODY
    // (post-`/`) in the parent buffer plus an alias for the pre-decode
    // source bytes. Phase B (bridge) and Phase C (codegen +
    // dispatcher) consume these to multi-site-rewrite non-UTF-8 name
    // tokens during PDF/A remediation. See the wave commit body for
    // the cohort and the upstream-cherry-pick plan.

    #[test]
    fn byte_range_at_buffer_start_excludes_leading_solidus() {
        // `/Hello` → body at [1, 6). 5-byte body matches "Hello".
        let bytes: &[u8] = b"/Hello";
        let name = parse(bytes);
        let range = name.byte_range().expect("parsed name carries byte_range");
        assert_eq!(range, 1..6);
        assert_eq!(&bytes[range.clone()], b"Hello");
        assert_eq!(range.end - range.start, name.source().len());
    }

    #[test]
    fn byte_range_after_offset_in_parent_buffer() {
        // Place a name several bytes into the buffer; byte_range must
        // be absolute to the buffer the Reader was constructed over.
        let bytes: &[u8] = b"<<\n/Key /Val>>";
        let mut r = Reader::new(bytes);
        // Skip `<<\n` so the reader sits at offset 3 (`/Key`).
        r.read_bytes(3).unwrap();
        let name = r.read_without_context::<Name<'_>>().unwrap();
        let range = name.byte_range().unwrap();
        // `/Key` starts at offset 3; body starts at offset 4.
        assert_eq!(range, 4..7);
        assert_eq!(&bytes[range], b"Key");
    }

    #[test]
    fn byte_range_for_escaped_name_covers_pre_decode_bytes() {
        // `#20` is 3 bytes pre-decode, 1 byte post-decode. byte_range
        // tracks the pre-decode span so callers can splice the
        // original token verbatim.
        let bytes: &[u8] = b"/lime#20Green";
        let name = parse(bytes);
        let range = name.byte_range().unwrap();
        assert_eq!(range, 1..13);
        assert_eq!(&bytes[range.clone()], b"lime#20Green");
        assert_eq!(range.end - range.start, name.source().len());
        // Post-decode value differs in length.
        assert_eq!(name.as_ref(), b"lime Green");
    }

    #[test]
    fn byte_range_for_empty_solidus_only_name() {
        // A bare `/` is a zero-length name; byte_range is an empty span
        // immediately past the solidus.
        let bytes: &[u8] = b"/";
        let name = parse(bytes);
        let range = name.byte_range().unwrap();
        assert_eq!(range, 1..1);
        assert!(name.source().is_empty());
    }

    #[test]
    fn byte_range_none_for_new_unescaped() {
        // Direct construction from a free-standing slice does NOT
        // carry a parent-buffer offset.
        let name = Name::new_unescaped(b"Hi");
        assert!(name.byte_range().is_none());
        assert_eq!(name.raw_bytes(), b"Hi");
    }

    #[test]
    fn byte_range_none_for_new_escaped() {
        let name = Name::new_escaped(b"lime#20Green").unwrap();
        assert!(name.byte_range().is_none());
        assert_eq!(name.raw_bytes(), b"lime#20Green");
        assert_eq!(name.as_ref(), b"lime Green");
    }

    #[test]
    fn byte_range_none_for_synthetic() {
        let name = Name::from_synthetic(b"Synthetic");
        assert!(name.byte_range().is_none());
        assert_eq!(name.raw_bytes(), b"");
    }

    #[test]
    fn raw_bytes_matches_source_for_parsed_name() {
        // raw_bytes() is an alias for source(); the two must always
        // return the same slice for parsed names.
        let name = parse(b"/AB#42cd");
        assert_eq!(name.raw_bytes(), name.source());
        assert_eq!(name.raw_bytes(), b"AB#42cd");
        assert_eq!(name.as_ref(), b"ABBcd");
    }

    #[test]
    fn byte_range_consistent_with_raw_bytes_length() {
        // Invariant: range.end - range.start == raw_bytes().len() for
        // every parsed name. Spot-check a few token shapes.
        for src in [
            &b"/Hi"[..],
            &b"/Hi#20there"[..],
            &b"/A;Name_With-Various***Characters?"[..],
            &b"/.notdef"[..],
        ] {
            let name = parse(src);
            let range = name.byte_range().unwrap();
            assert_eq!(
                range.end - range.start,
                name.raw_bytes().len(),
                "byte_range / raw_bytes mismatch for {src:?}"
            );
            assert_eq!(&src[range], name.raw_bytes());
        }
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn byte_range_retains_offset_at_u32_max_but_drops_beyond() {
        // The packed offset is a `u32`, so an offset of exactly
        // `u32::MAX` still fits and is RETAINED; only offsets that do
        // not fit in a `u32` (> `u32::MAX`, i.e. ≥ 2^32) collapse to
        // `None`. Pins the boundary documented on `SourceOffset` /
        // `new_at` — a >4 GiB scenario, hence 64-bit only.
        let data: &[u8] = b"Foo";
        let max = u32::MAX as usize;

        let at_max = Name::new_at(data, Some(max)).unwrap();
        assert_eq!(at_max.byte_range(), Some(max..max + data.len()));

        let beyond = Name::new_at(data, Some(max + 1)).unwrap();
        assert!(beyond.byte_range().is_none());
    }
}

// --- L1: byte_range is file-absolute ONLY for names lexed directly from
// the file buffer; detached (sub-reader / decoded / streamed) contexts
// must report `None` rather than an offset into an ephemeral buffer. ---
#[cfg(test)]
mod byte_range_file_buffer_tests {
    use crate::Pdf;
    use crate::object::{Array, Dict, Name, ObjectIdentifier};
    use alloc::format;
    use alloc::vec::Vec;

    /// A minimal single-revision PDF whose catalog (obj 1) carries a name
    /// value (`/Marker /Alpha`) and a name array (`/Arr [/Beta /Gamma]`),
    /// so the dict-key / dict-value / array-element read paths can each be
    /// exercised.
    fn build_pdf() -> Vec<u8> {
        let mut pdf: Vec<u8> = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.7\n");
        let off1 = pdf.len();
        pdf.extend_from_slice(
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Marker /Alpha /Arr [/Beta /Gamma] >>\nendobj\n",
        );
        let off2 = pdf.len();
        pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        let xref_pos = pdf.len();
        pdf.extend_from_slice(b"xref\n0 3\n");
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        pdf.extend_from_slice(format!("{off1:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off2:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(b"trailer\n<< /Size 3 /Root 1 0 R >>\n");
        pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF").as_bytes());
        pdf
    }

    fn catalog(pdf: &Pdf) -> Dict<'_> {
        pdf.xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(1, 0))
            .expect("catalog dict")
    }

    #[test]
    fn dict_key_from_file_buffer_has_absolute_byte_range() {
        // A dictionary key of a top-level resident object IS lexed directly
        // from the file buffer, so its byte_range is a valid file position.
        let bytes = build_pdf();
        let pdf = Pdf::new(bytes.clone()).expect("pdf loads");
        let dict = catalog(&pdf);
        let key = dict
            .keys()
            .find(|k| k.as_ref() == b"Marker")
            .expect("Marker key");
        let range = key
            .byte_range()
            .expect("top-level dict key is file-absolute");
        assert_eq!(&bytes[range], b"Marker");
    }

    #[test]
    fn dict_key_via_reference_resolution_has_absolute_byte_range() {
        // Reaching a top-level object THROUGH a reference — `catalog.get::
        // <Dict>("Pages")` resolves `2 0 R` — must not poison its own keys'
        // byte_range. Obj 2 is lexed from the whole-file buffer at its true
        // offset, so its keys stay file-absolute even though the referring
        // catalog's forwarded value context is detached. Regression:
        // `get_with`'s resident Normal branch must clear `detached`, not
        // just `in_object_stream`.
        let bytes = build_pdf();
        let pdf = Pdf::new(bytes.clone()).expect("pdf loads");
        let pages: Dict<'_> = catalog(&pdf)
            .get::<Dict<'_>>(b"Pages")
            .expect("Pages dict via 2 0 R");
        let key = pages
            .keys()
            .find(|k| k.as_ref() == b"Type")
            .expect("Type key");
        let range = key
            .byte_range()
            .expect("via-ref top-level dict key must stay file-absolute");
        assert_eq!(&bytes[range], b"Type");
    }

    #[test]
    fn dict_value_name_has_no_byte_range() {
        // A name read as a dict VALUE comes from a fresh sub-reader over
        // `&data[offset..]`, so its offset is not a file position.
        let bytes = build_pdf();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let dict = catalog(&pdf);
        let value: Name<'_> = dict.get::<Name<'_>>(b"Marker").expect("Marker value");
        assert_eq!(value.as_ref(), b"Alpha");
        assert!(
            value.byte_range().is_none(),
            "a dict-value name is lexed from a sub-reader, not the file buffer"
        );
    }

    #[test]
    fn array_element_name_has_no_byte_range() {
        // A name read as an array element likewise comes from a sub-reader.
        let bytes = build_pdf();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let dict = catalog(&pdf);
        let arr: Array<'_> = dict.get::<Array<'_>>(b"Arr").expect("Arr array");
        let first: Name<'_> = arr.iter::<Name<'_>>().next().expect("first name element");
        assert_eq!(first.as_ref(), b"Beta");
        assert!(
            first.byte_range().is_none(),
            "an array-element name is lexed from a sub-reader, not the file buffer"
        );
    }

    #[test]
    fn object_stream_name_has_no_byte_range() {
        // In `catalog-in-objstm-aes.pdf` the catalog lives inside a
        // `/Type /ObjStm` object stream; names lexed from the decoded
        // stream buffer must not report a file-absolute byte_range.
        let bytes: &[u8] =
            include_bytes!("../../../beeld-tests/pdfs/custom/catalog-in-objstm-aes.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("objstm fixture loads");
        let dict = pdf
            .xref()
            .get::<Dict<'_>>(pdf.xref().root_id())
            .expect("catalog dict from object stream");
        let mut checked = 0;
        for key in dict.keys() {
            assert!(
                key.byte_range().is_none(),
                "object-stream dict key {:?} must have no file-absolute byte_range",
                key.as_str()
            );
            checked += 1;
        }
        assert!(checked > 0, "fixture catalog must expose at least one key");
    }

    #[test]
    fn streamed_source_name_has_no_byte_range() {
        // The same catalog parsed from a positioned-read (streamed) source
        // is read via an object window, not the resident file buffer, so
        // its keys must not report a file-absolute byte_range.
        struct SliceReadAt(Vec<u8>);
        impl crate::ReadAt for SliceReadAt {
            fn len(&self) -> u64 {
                self.0.len() as u64
            }
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, crate::ReadAtError> {
                let start = (offset as usize).min(self.0.len());
                let avail = &self.0[start..];
                let n = avail.len().min(buf.len());
                buf[..n].copy_from_slice(&avail[..n]);
                Ok(n)
            }
        }

        let pdf = Pdf::new_with_reader(SliceReadAt(build_pdf())).expect("streamed pdf loads");
        let dict = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(1, 0))
            .expect("catalog dict");
        let mut checked = 0;
        for key in dict.keys() {
            assert!(
                key.byte_range().is_none(),
                "streamed-window dict key {:?} must have no file-absolute byte_range",
                key.as_str()
            );
            checked += 1;
        }
        assert!(checked > 0, "catalog must expose at least one key");
    }
}

// --- L2: under a bare-`Reader` / `from_bytes` (Dummy) context, dict-value
// and array-element names are re-read from a fresh sub-reader over an offset
// sub-slice, so their `byte_range` must be `None` — never a sub-slice-
// relative offset masquerading as a position in the caller's buffer. The
// top-level KEY, lexed directly from that buffer, keeps a `Some` span. ---
#[cfg(test)]
mod byte_range_dummy_context_tests {
    use crate::object::{Array, Dict, FromBytes, Name};

    #[test]
    fn dummy_dict_key_keeps_byte_range() {
        // The key IS lexed directly from the caller's `from_bytes` buffer,
        // so its span is meaningful against that buffer and stays `Some`.
        let buf: &[u8] = b"<< /K /Val >>";
        let dict = Dict::from_bytes(buf).expect("dict parses");
        let key = dict.keys().find(|k| k.as_ref() == b"K").expect("K key");
        let range = key.byte_range().expect("top-level key span");
        assert_eq!(&buf[range], b"K");
    }

    #[test]
    fn dummy_dict_value_name_has_no_byte_range() {
        // The value is re-read from a sub-reader over `&data[offset..]`, so
        // its recorded offset is not a position in `buf`; must be `None`.
        let buf: &[u8] = b"<< /K /Val >>";
        let dict = Dict::from_bytes(buf).expect("dict parses");
        let value: Name<'_> = dict.get::<Name<'_>>(b"K").expect("K value");
        assert_eq!(value.as_ref(), b"Val");
        assert!(
            value.byte_range().is_none(),
            "a Dummy-context dict-value name is lexed from a sub-reader"
        );
    }

    #[test]
    fn dummy_array_element_name_has_no_byte_range() {
        let buf: &[u8] = b"[/Aaa /Bbb]";
        let arr = Array::from_bytes(buf).expect("array parses");
        let first: Name<'_> = arr.iter::<Name<'_>>().next().expect("first element");
        assert_eq!(first.as_ref(), b"Aaa");
        assert!(
            first.byte_range().is_none(),
            "a Dummy-context array-element name is lexed from a sub-reader"
        );
    }

    #[test]
    fn dummy_nested_dict_key_has_no_byte_range() {
        // The inner dict is read by `get_raw` from a sub-reader whose context
        // is ALREADY detached (it is the outer dict's stored value context).
        // `read_inner` must keep the inner stored context detached whether it
        // re-sets the flag or inherits it from the clone, so the nested key is
        // lexed detached and reports `None` — never a sub-slice offset.
        let buf: &[u8] = b"<< /Outer << /Inner /V >> >>";
        let dict = Dict::from_bytes(buf).expect("outer dict parses");
        let inner: Dict<'_> = dict.get::<Dict<'_>>(b"Outer").expect("inner dict value");
        let key = inner
            .keys()
            .find(|k| k.as_ref() == b"Inner")
            .expect("Inner key");
        assert!(
            key.byte_range().is_none(),
            "a nested-dict key under an already-detached context is not file-absolute"
        );
    }
}
