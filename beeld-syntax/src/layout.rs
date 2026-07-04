//! Physical file layout inspection.
//!
//! This module surfaces byte-level information about the PDF file's outer
//! structure — the position of the `%PDF-` header, the four binary-marker
//! bytes following it, every `%%EOF` offset, and the count of bytes
//! trailing the last `%%EOF` — plus per-object layout (byte range,
//! keyword canonicity). Conformance and repair tooling needs this
//! information; pure rendering does not.
//!
//! Gated on the `inspect` feature, per §1.4 of the fork roadmap.

use crate::object::{Object, ObjectIdentifier};
use crate::reader::{Reader, ReaderContext, ReaderExt};
use alloc::vec::Vec;
use core::ops::Range;

/// File-level physical layout of a PDF document.
///
/// All offsets are byte positions into the original PDF source. Produced
/// by [`crate::Pdf::file_layout`]. Requires the `inspect` feature.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileLayout {
    /// Byte offset of the `%PDF-` header within the file.
    ///
    /// ISO 32000-1 §7.5.2 permits a preamble in some profiles; others
    /// (e.g. PDF/A) require this to be `0`.
    pub header_offset: usize,
    /// The four bytes of the binary-identification comment that may
    /// follow the header line (ISO 32000-1 §7.5.2). `None` if the
    /// comment is absent, shorter than four bytes, or does not consist
    /// of four high-bit-set bytes as the specification requires.
    pub binary_marker: Option<[u8; 4]>,
    /// Offsets of every `%%EOF` marker, in file order.
    pub eof_offsets: Vec<usize>,
    /// Bytes following the last `%%EOF`, excluding at most one trailing
    /// EOL marker (`\n`, `\r`, or `\r\n`). Zero when nothing (or only a
    /// single EOL) follows the final marker.
    pub post_eof_bytes: usize,
    /// Total file size in bytes.
    pub file_size: usize,
}

const PDF_HEADER: &[u8] = b"%PDF-";
const EOF_MARKER: &[u8] = b"%%EOF";

impl FileLayout {
    /// Compute a [`FileLayout`] by scanning the raw PDF bytes.
    ///
    /// A single linear pass. The caller is expected to cache the result
    /// rather than recomputing it.
    pub(crate) fn compute(data: &[u8]) -> Self {
        let file_size = data.len();
        let header_offset = find_subslice(data, PDF_HEADER).unwrap_or(0);
        let binary_marker = scan_binary_marker(data, header_offset);
        let eof_offsets = find_all(data, EOF_MARKER);
        let post_eof_bytes = match eof_offsets.last() {
            Some(&last) => {
                let after = last + EOF_MARKER.len();
                // Subtract at most one trailing EOL (CR, LF, or CRLF).
                let tail = &data[after.min(file_size)..];
                match tail {
                    [b'\r', b'\n', rest @ ..] => rest.len(),
                    [b'\r', rest @ ..] | [b'\n', rest @ ..] => rest.len(),
                    _ => tail.len(),
                }
            }
            None => 0,
        };
        Self {
            header_offset,
            binary_marker,
            eof_offsets,
            post_eof_bytes,
            file_size,
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || haystack.len() < needle.len() {
        return out;
    }
    let mut i = 0;
    let end = haystack.len() - needle.len() + 1;
    while i < end {
        if &haystack[i..i + needle.len()] == needle {
            out.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

fn scan_binary_marker(data: &[u8], header_offset: usize) -> Option<[u8; 4]> {
    // Walk to the end of the header line.
    let mut i = header_offset;
    while i < data.len() && data[i] != b'\n' && data[i] != b'\r' {
        i += 1;
    }
    // Skip the end-of-line marker (CRLF, CR, or LF).
    match data.get(i) {
        Some(b'\r') => {
            i += 1;
            if data.get(i) == Some(&b'\n') {
                i += 1;
            }
        }
        Some(b'\n') => {
            i += 1;
        }
        _ => return None,
    }
    // The binary-identification comment begins with `%`.
    if data.get(i) != Some(&b'%') {
        return None;
    }
    i += 1;
    // Next four bytes must all have the high bit set per ISO 32000-1 §7.5.2.
    let four = data.get(i..i.checked_add(4)?)?;
    if four.iter().all(|&b| b >= 128) {
        Some([four[0], four[1], four[2], four[3]])
    } else {
        None
    }
}

/// Physical layout of an indirect object that lives directly in the PDF.
///
/// Captures the `N G obj … endobj` span and three canonical-form flags
/// used by conformance tooling that validates §7.3.10 object layout.
/// Produced by [`crate::xref::XRef::indirect_layout`]. Requires the
/// `inspect` feature.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct IndirectLayout {
    /// Byte range of the whole `N G obj … endobj` header-through-keyword.
    pub range: Range<usize>,
    /// `true` when the header is `N<SP>G<SP>obj<EOL>` with exactly one
    /// ASCII SP (`0x20`) between `N` and `G`, one between `G` and
    /// `obj`, and `obj` followed by a single EOL (LF / CR / CRLF).
    pub header_canonical: bool,
    /// `true` when `endobj` is preceded by an EOL marker
    /// (LF / CR / CRLF).
    pub endobj_preceded_by_eol: bool,
    /// `true` when `endobj` is followed by an EOL marker or EOF.
    pub endobj_followed_by_eol: bool,
}

/// Result of resolving an object identifier against the xref for layout
/// information.
///
/// Every case of an xref entry has its own variant so callers do not
/// need to pattern-match on `Option<..>` and re-resolve the
/// compressed-object hop themselves. Requires the `inspect` feature.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayoutKind {
    /// Object is stored directly in the PDF; `layout` describes its
    /// `N G obj … endobj` span.
    Direct(IndirectLayout),
    /// Object is stored compressed inside an object stream.
    Compressed {
        /// Object identifier of the hosting object stream.
        host: ObjectIdentifier,
        /// Layout of the hosting object stream itself. Object streams
        /// always live directly in the PDF.
        host_layout: IndirectLayout,
        /// Zero-based position of the object inside the stream.
        index: u32,
    },
    /// Object is a free entry on the free list.
    Free,
}

/// Scan the indirect-object span starting at `offset` in `data`.
///
/// Returns `None` if the header does not parse as `N<whitespace>G<whitespace>obj`
/// or if no `endobj` can be found. Header-canonicity and EOL flags are
/// populated per [`IndirectLayout`].
///
/// The object's value is parsed structurally by the canonical object
/// reader, so the `endobj` search resumes past the whole value rather
/// than stopping at a false `endobj` byte sequence occurring inside a
/// string, name, or stream body (ISO 32000-1 §7.3.4, §7.3.5, §7.3.8.2).
/// For a stream the skipped body is bounded by the stream's `/Length`,
/// and stream-ness is decided by the reader — a `stream` keyword
/// following the object's top-level dictionary — not by an incidental
/// `stream` token elsewhere in the body. When `/Length` is absent,
/// indirect, or wrong, the reader's recovery path bounds the body at the
/// first `endstream` sequence, so a crafted body can still spoof the
/// terminator — the residual §7.3.8 limitation documented on
/// `Stream::keyword_body_range`.
pub(crate) fn scan_indirect_layout(data: &[u8], offset: usize) -> Option<IndirectLayout> {
    let tail = data.get(offset..)?;

    let (header_canonical, header_end) = parse_indirect_header(tail)?;
    let body_start = offset + header_end;

    // Locate the `endobj` that closes this object. The value may contain
    // the literal bytes `endobj` — inside a string, a name, or a stream's
    // `/Length`-bounded binary body — so parse it structurally and resume
    // the search past the whole parsed value rather than inside it. A value
    // that does not parse (or a bare keyword / empty body) is searched from
    // the body start.
    let search_from = value_end(data, body_start).unwrap_or(body_start);
    let endobj_pos = search_from + find_subslice(data.get(search_from..)?, b"endobj")?;
    let endobj_end = endobj_pos + b"endobj".len();

    // The byte immediately before `endobj` — relative to the full data
    // slice, not to `tail` — determines the preceding-EOL flag.
    let endobj_preceded_by_eol =
        endobj_pos > 0 && matches!(data.get(endobj_pos - 1), Some(b'\n') | Some(b'\r'));
    let endobj_followed_by_eol = match data.get(endobj_end) {
        None => true,
        Some(b'\n') | Some(b'\r') => true,
        Some(_) => false,
    };

    Some(IndirectLayout {
        range: offset..endobj_end,
        header_canonical,
        endobj_preceded_by_eol,
        endobj_followed_by_eol,
    })
}

/// Return the byte offset just past the object value beginning at
/// `value_start` in `data`, so the `endobj` search can resume beyond it.
///
/// The value is parsed by the canonical object reader, which consumes the
/// whole value — dict, stream, array, string, or name — so the returned
/// offset lies past any `endobj` bytes embedded in a string, name, or
/// (`/Length`-bounded) stream body (ISO 32000-1 §7.3.4, §7.3.5, §7.3.6,
/// §7.3.8.2). Stream-ness is decided by the reader — a `stream` keyword
/// following the object's top-level dictionary (ISO 32000-1 §7.3.8) — not
/// by an incidental `stream` token inside a name or string.
///
/// Returns `None` — so the caller searches from the body start — for a
/// value that does not parse, and for a `null` value. The `Null` arm also
/// covers the reader's lenient operator-like fallback: for a body that is
/// not a real value (e.g. `endobj` directly, an empty object) it yields
/// `Null` after consuming a bare keyword whose end may lie *past* the real
/// terminator, so that offset must not be trusted. A `null` value embeds no
/// `endobj` bytes, so resuming from the body start is equally correct
/// for it.
fn value_end(data: &[u8], value_start: usize) -> Option<usize> {
    let mut reader = Reader::new(data);
    reader.jump(value_start);
    // Mirror `IndirectObject::read`: the value may be preceded by
    // whitespace or a comment (e.g. `obj <<`, `obj\n\n<<`, `obj\n%c\n<<`),
    // which `Object::read` does not skip. Without this the value reads as
    // `Null` and a genuine stream/dict is missed.
    reader.skip_white_spaces_and_comments();
    match reader.read_with_context::<Object<'_>>(&ReaderContext::dummy())? {
        Object::Null(_) => None,
        _ => Some(reader.offset()),
    }
}

/// Parse an `N G obj` header.
///
/// Returns `(canonical, bytes_consumed)` on success. `canonical` is
/// `true` iff exactly one ASCII SP separates each token and `obj` is
/// followed by a single EOL marker.
fn parse_indirect_header(data: &[u8]) -> Option<(bool, usize)> {
    let mut i = 0;

    // N (digits)
    let n_start = i;
    while matches!(data.get(i), Some(b'0'..=b'9')) {
        i += 1;
    }
    if i == n_start {
        return None;
    }

    // One SP between N and G (canonical); otherwise accept any whitespace run.
    let sp1_canonical = data.get(i) == Some(&b' ');
    let (skipped_1, i2) = skip_ws_counting(data, i);
    if skipped_1 == 0 {
        return None;
    }
    i = i2;

    // G (digits)
    let g_start = i;
    while matches!(data.get(i), Some(b'0'..=b'9')) {
        i += 1;
    }
    if i == g_start {
        return None;
    }

    // One SP between G and obj (canonical).
    let sp2_canonical = data.get(i) == Some(&b' ');
    let (skipped_2, i3) = skip_ws_counting(data, i);
    if skipped_2 == 0 {
        return None;
    }
    i = i3;

    // `obj` keyword.
    if data.get(i..i + 3)? != b"obj" {
        return None;
    }
    i += 3;

    // Single EOL (CR / LF / CRLF).
    let eol_ok = match data.get(i) {
        Some(b'\r') => {
            i += 1;
            if data.get(i) == Some(&b'\n') {
                i += 1;
            }
            true
        }
        Some(b'\n') => {
            i += 1;
            true
        }
        _ => false,
    };

    let canonical = sp1_canonical && skipped_1 == 1 && sp2_canonical && skipped_2 == 1 && eol_ok;
    Some((canonical, i))
}

fn skip_ws_counting(data: &[u8], start: usize) -> (usize, usize) {
    let mut count = 0;
    let mut i = start;
    while matches!(
        data.get(i),
        Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
    ) {
        count += 1;
        i += 1;
    }
    (count, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_at_offset_zero() {
        let data = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n1 0 obj\nendobj\n%%EOF\n";
        let layout = FileLayout::compute(data);
        assert_eq!(layout.header_offset, 0);
        assert_eq!(layout.binary_marker, Some([0xe2, 0xe3, 0xcf, 0xd3]));
        assert_eq!(layout.eof_offsets.len(), 1);
        assert_eq!(layout.post_eof_bytes, 0);
        assert_eq!(layout.file_size, data.len());
    }

    #[test]
    fn header_offset_with_preamble() {
        let preamble: Vec<u8> = core::iter::repeat_n(b'X', 128).collect();
        let mut data = preamble;
        data.extend_from_slice(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n%%EOF\n");
        let layout = FileLayout::compute(&data);
        assert_eq!(layout.header_offset, 128);
        assert_eq!(layout.binary_marker, Some([0xe2, 0xe3, 0xcf, 0xd3]));
    }

    #[test]
    fn post_eof_trailing_garbage_is_counted() {
        let mut data: Vec<u8> = b"%PDF-1.7\n%%EOF\n".to_vec();
        data.extend(core::iter::repeat_n(b'X', 256));
        let layout = FileLayout::compute(&data);
        assert_eq!(layout.post_eof_bytes, 256);
    }

    #[test]
    fn post_eof_without_trailing_eol_counts_everything() {
        // Last %%EOF has no trailing newline.
        let data = b"%PDF-1.7\n%%EOF";
        let layout = FileLayout::compute(data);
        assert_eq!(layout.post_eof_bytes, 0);
    }

    #[test]
    fn post_eof_strips_crlf_only_once() {
        let data = b"%PDF-1.7\n%%EOF\r\nabc";
        let layout = FileLayout::compute(data);
        assert_eq!(layout.post_eof_bytes, 3);
    }

    #[test]
    fn binary_marker_absent_without_comment_line() {
        // No binary comment after the header.
        let data = b"%PDF-1.7\n1 0 obj\nendobj\n%%EOF\n";
        let layout = FileLayout::compute(data);
        assert!(layout.binary_marker.is_none());
    }

    #[test]
    fn binary_marker_absent_with_low_ascii_comment() {
        // Has a comment but fewer than four high-bit bytes after `%`.
        let data = b"%PDF-1.7\n%abcd\n%%EOF\n";
        let layout = FileLayout::compute(data);
        assert!(layout.binary_marker.is_none());
    }

    #[test]
    fn binary_marker_absent_with_three_high_bit_bytes() {
        // Only three high-bit bytes — not enough for the spec requirement.
        let data = b"%PDF-1.7\n%\xe2\xe3\xcfa\n%%EOF\n";
        let layout = FileLayout::compute(data);
        assert!(layout.binary_marker.is_none());
    }

    #[test]
    fn multiple_eof_markers_are_all_reported() {
        let data = b"%PDF-1.7\n1 0 obj\nendobj\n%%EOF\n2 0 obj\nendobj\n%%EOF\n";
        let layout = FileLayout::compute(data);
        assert_eq!(layout.eof_offsets.len(), 2);
        assert!(layout.eof_offsets[0] < layout.eof_offsets[1]);
    }

    #[test]
    fn no_eof_marker_yields_zero_post_eof_bytes() {
        let data = b"%PDF-1.7\n1 0 obj\nendobj\n";
        let layout = FileLayout::compute(data);
        assert!(layout.eof_offsets.is_empty());
        assert_eq!(layout.post_eof_bytes, 0);
    }

    #[test]
    fn crlf_header_line_handled() {
        // Some PDFs use CRLF in the header line.
        let data = b"%PDF-1.7\r\n%\xe2\xe3\xcf\xd3\r\n%%EOF\r\n";
        let layout = FileLayout::compute(data);
        assert_eq!(layout.binary_marker, Some([0xe2, 0xe3, 0xcf, 0xd3]));
    }

    #[test]
    fn binary_marker_boundary_of_file() {
        // Binary marker truncated at file end — must not read past the buffer.
        let data = b"%PDF-1.7\n%\xe2\xe3\xcf";
        let layout = FileLayout::compute(data);
        assert!(layout.binary_marker.is_none());
    }

    // --- indirect-object layout scanner -------------------------------

    #[test]
    fn canonical_indirect_layout() {
        let data = b"5 0 obj\n(hi)\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert!(layout.header_canonical);
        assert!(layout.endobj_preceded_by_eol);
        assert!(layout.endobj_followed_by_eol);
        assert_eq!(layout.range.start, 0);
        assert_eq!(layout.range.end, data.len() - 1); // minus final \n
    }

    #[test]
    fn tab_between_n_and_g_is_not_canonical() {
        let data = b"5\t0 obj\n(hi)\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert!(!layout.header_canonical);
    }

    #[test]
    fn two_spaces_between_tokens_is_not_canonical() {
        let data = b"5  0 obj\n(hi)\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert!(!layout.header_canonical);
    }

    #[test]
    fn endobj_without_trailing_eol_flag_false() {
        // `endobj` immediately followed by the next `N G obj`.
        let data = b"5 0 obj\n(hi)\nendobj6 0 obj\n(x)\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert!(!layout.endobj_followed_by_eol);
    }

    #[test]
    fn endobj_without_preceding_eol_flag_false() {
        // `endobj` directly after content with no EOL.
        let data = b"5 0 obj\n(hi)endobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert!(!layout.endobj_preceded_by_eol);
        assert!(layout.endobj_followed_by_eol);
    }

    #[test]
    fn header_missing_obj_keyword_rejected() {
        let data = b"5 0 xyz\n(hi)\nendobj\n";
        assert!(scan_indirect_layout(data, 0).is_none());
    }

    #[test]
    fn header_without_eol_after_obj_not_canonical() {
        let data = b"5 0 obj(hi)endobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert!(!layout.header_canonical);
    }

    #[test]
    fn offset_into_middle_of_buffer_works() {
        let data = b"garbage5 0 obj\n(hi)\nendobj\nmore";
        let layout = scan_indirect_layout(data, 7).expect("layout");
        assert!(layout.header_canonical);
        assert_eq!(layout.range.start, 7);
        // endobj ends just before `\nmore` → offset of `\n` is 7 + 7 + "(hi)\n".len() + "endobj".len()
        // Actual: 7 + 13 (header) + 5 ("(hi)\n") + 6 ("endobj") — tricky, just assert bounds.
        assert!(data[layout.range.clone()].ends_with(b"endobj"));
    }

    #[test]
    fn endobj_inside_stream_body_is_skipped() {
        // The stream body contains the literal bytes `endobj`. The scanner
        // must skip the body (stream .. endstream) and take the real
        // `endobj` after `endstream`, not the false one inside the body.
        let data = b"5 0 obj\n<< /Length 8 >>\nstream\nXendobjX\nendstream\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        // The real `endobj` ends just before the final trailing `\n`.
        assert_eq!(layout.range.end, data.len() - 1);
        assert!(data[layout.range.clone()].ends_with(b"endobj"));
        // The span must cover the whole stream (proving the in-body `endobj`
        // was not taken as the boundary).
        assert!(find_subslice(&data[layout.range.clone()], b"endstream").is_some());
    }

    #[test]
    fn non_stream_object_layout_unaffected() {
        // A non-stream object whose file is followed by a later stream
        // object: the trailing `stream` keyword must not be treated as this
        // object's body.
        let data = b"5 0 obj\n<< /A 1 >>\nendobj\n6 0 obj\n<< /Length 1 >>\nstream\nx\nendstream\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert!(data[layout.range.clone()].ends_with(b"endobj"));
        // Bounded to object 5: the range must end before object 6's header.
        let obj6 = find_subslice(data, b"6 0 obj").expect("obj 6");
        assert!(layout.range.end <= obj6);
    }

    #[test]
    fn stream_body_endstream_endobj_bytes_bounded_by_length() {
        // A stream whose body — bounded by a correct `/Length` — itself
        // contains the byte sequences `endstream` then `endobj`. The scan
        // must skip the whole `/Length`-sized body (not stop at the first
        // in-body `endstream`/`endobj`) and take the real terminator, per
        // ISO 32000-1 §7.3.8.2. Regression pin for the `/Length`-ignoring
        // first-`endstream` text search.
        let data = b"5 0 obj\n<< /Length 21 >>\nstream\nAAendstreamBBendobjCC\nendstream\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        // Ends at the real `endobj`, just before the final trailing `\n`.
        assert_eq!(layout.range.end, data.len() - 1);
        assert!(data[layout.range.clone()].ends_with(b"endobj"));
        // The span covers the real `endstream`, proving neither the in-body
        // `endstream` nor the in-body `endobj` was taken as the boundary.
        let real_endstream = find_subslice(data, b"\nendstream\n").expect("real endstream");
        assert!(layout.range.end > real_endstream);
    }

    #[test]
    fn endobj_in_dict_string_before_stream_is_not_the_boundary() {
        // A genuine stream whose dictionary carries the bytes `endobj`
        // inside a string, before the `stream` keyword. The scan must not
        // mistake that in-dictionary `endobj` for the object boundary; the
        // body is `/Length`-bounded and the real `endobj` follows
        // `endstream`.
        let data = b"5 0 obj\n<< /X (endobj) /Length 3 >>\nstream\nabc\nendstream\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert_eq!(layout.range.end, data.len() - 1);
        assert!(data[layout.range.clone()].ends_with(b"endobj"));
        assert!(find_subslice(&data[layout.range.clone()], b"endstream").is_some());
    }

    #[test]
    fn non_stream_string_containing_stream_token_is_not_a_stream() {
        // A non-stream object whose string value ends a line with the token
        // `stream`. `value_end` classifies via the canonical object reader,
        // so the value reads as a `String` — not a `Stream` — and the
        // `endobj` search resumes just past the string, reaching the real
        // `endobj`.
        let data = b"1 0 obj\n(a stream\nx)\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert_eq!(layout.range.end, data.len() - 1);
        assert!(data[layout.range.clone()].ends_with(b"endobj"));

        // A second form: the `stream` token sits mid-string.
        let data = b"5 0 obj\n(see stream\nmore)\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert_eq!(layout.range.end, data.len() - 1);
        assert!(data[layout.range.clone()].ends_with(b"endobj"));
    }

    #[test]
    fn endobj_in_nonstream_dict_string_is_not_the_boundary() {
        // A plain (non-stream) dictionary object carrying the bytes `endobj`
        // inside a string value. The value is parsed structurally, so the
        // search resumes past `>>` and takes the real terminator — not the
        // false `endobj` inside the string (ISO 32000-1 §7.3.4).
        let data = b"1 0 obj\n<< /X (endobj) >>\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert_eq!(layout.range.end, data.len() - 1);
        assert!(data[layout.range.clone()].ends_with(b"endobj"));
    }

    #[test]
    fn endobj_in_nonstream_name_value_is_not_the_boundary() {
        // A name value whose characters spell `endobj`. Parsed structurally,
        // the search resumes past the name rather than stopping at the
        // in-name `endobj` (ISO 32000-1 §7.3.5).
        let data = b"1 0 obj\n/endobj\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert_eq!(layout.range.end, data.len() - 1);
        assert!(data[layout.range.clone()].ends_with(b"endobj"));
    }

    #[test]
    fn endobj_in_nonstream_array_string_is_not_the_boundary() {
        // An array value carrying the bytes `endobj` inside a nested string.
        // The whole array is consumed, so the false in-array `endobj` is not
        // taken as the boundary (ISO 32000-1 §7.3.6).
        let data = b"1 0 obj\n[ (endobj) ]\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert_eq!(layout.range.end, data.len() - 1);
        assert!(data[layout.range.clone()].ends_with(b"endobj"));
    }

    #[test]
    fn object_with_no_value_body_resolves_to_endobj() {
        // No value between `obj` and `endobj`. The object reader's lenient
        // operator-like fallback consumes the `endobj` keyword itself and
        // yields `Null`; the scanner must not trust that offset (it lies past
        // the real terminator) and must instead resolve to the real `endobj`.
        let data = b"1 0 obj\nendobj\n";
        let layout = scan_indirect_layout(data, 0).expect("layout");
        assert_eq!(layout.range.end, data.len() - 1);
        assert!(data[layout.range.clone()].ends_with(b"endobj"));
    }

    #[test]
    fn stream_with_false_endobj_body_detected_despite_noncanonical_obj_spacing() {
        // A genuine stream whose `/Length`-bounded body carries the literal
        // bytes `endobj`. When the `N G obj` header is not the common
        // `obj\n<<` form — a space after `obj`, more than one EOL, or an
        // interposed comment line — the dictionary still opens the body, so
        // `value_end` must skip that leading whitespace/comment (as
        // the canonical `IndirectObject::read` does) before classifying the
        // value. Without the skip the value reads as `Null`, the stream is
        // missed, and the `endobj` search falls back to `body_start` and
        // stops at the false in-body `endobj`, truncating the range.
        let variants: [&[u8]; 3] = [
            b"5 0 obj << /Length 8 >>\nstream\nXendobjX\nendstream\nendobj\n",
            b"5 0 obj\n\n<< /Length 8 >>\nstream\nXendobjX\nendstream\nendobj\n",
            b"5 0 obj\n%c\n<< /Length 8 >>\nstream\nXendobjX\nendstream\nendobj\n",
        ];
        for data in variants {
            let layout = scan_indirect_layout(data, 0).expect("layout");
            // Reaches the real terminating `endobj`, just before the final EOL.
            assert_eq!(layout.range.end, data.len() - 1);
            assert!(data[layout.range.clone()].ends_with(b"endobj"));
            // The span covers the real `endstream`, proving the in-body
            // `endobj` was not taken as the boundary.
            assert!(find_subslice(&data[layout.range.clone()], b"endstream").is_some());
        }
    }
}
