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

use crate::object::ObjectIdentifier;
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
/// or if `endobj` is not found. Header-canonicity and EOL flags are
/// populated per [`IndirectLayout`].
pub(crate) fn scan_indirect_layout(data: &[u8], offset: usize) -> Option<IndirectLayout> {
    let tail = data.get(offset..)?;

    let (header_canonical, header_end) = parse_indirect_header(tail)?;

    // Find `endobj` in the body after the header.
    let endobj_rel = find_subslice(&tail[header_end..], b"endobj")? + header_end;
    let endobj_pos = offset + endobj_rel;
    let endobj_end = endobj_pos + b"endobj".len();

    // The byte immediately before `endobj` — relative to the full data
    // slice, not to `tail` — determines the preceding-EOL flag.
    let endobj_preceded_by_eol = endobj_pos > 0
        && matches!(data.get(endobj_pos - 1), Some(b'\n') | Some(b'\r'));
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
}
