//! Linearization (web-optimised) parameter dictionary.
//!
//! ISO 32000-1 Annex F describes linearized PDFs: the first indirect
//! object is a parameter dictionary whose `/Linearized` key announces
//! the format version, and the file has a first-page trailer near the
//! head that lets a network reader render page 1 before downloading
//! the remainder.
//!
//! This module exposes the parameter dict; the [`hint`] submodule decodes
//! the hint tables it points to (ISO 32000-2 Annex F.4).

use crate::object::{Dict, ObjectIdentifier};
use crate::xref::{EntryType, XRef};
use alloc::vec::Vec;

pub mod hint;

pub use hint::{
    HintTables, PageEntry, PageOffsetHeader, PageOffsetTable, SharedGroup, SharedObjectHeader,
    SharedObjectTable, SharedRef,
};

/// Linearization parameter data (ISO 32000-1 Annex F).
///
/// All byte offsets are positions into the original PDF source and are
/// narrowed from the file's `i64` dict values to `usize` at parse time.
/// If any required integer does not fit in `usize` on this target, the
/// parameter dict is reported as
/// [`LinearizationKind::Malformed`] with
/// [`LinearizationError::OffsetTooLarge`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Linearization {
    /// Value of the `/Linearized` key — the format version (typically 1.0).
    pub version: f64,
    /// Value of `/L` — the original file size.
    pub length: usize,
    /// Value of `/O` — the first page's object number.
    pub first_page_object: i32,
    /// Value of `/E` — the offset of the end of the first page, relative
    /// to the beginning of the PDF file (Table F.1).
    pub first_page_end_offset: usize,
    /// Value of `/T` — the offset of the white-space character preceding
    /// object 0 of the main cross-reference table (standard and hybrid
    /// files), or the offset of the main cross-reference *stream object*
    /// for files that use cross-reference streams exclusively. Distinct
    /// from the first-page trailer's `/Prev`, which locates the `xref`
    /// line preceding that table (Table F.1).
    pub main_xref_offset: usize,
    /// Value of `/N` — number of pages.
    pub page_count: i32,
    /// Value of `/H` — the primary hint stream's offset and length, and
    /// optionally an overflow hint stream's: an array of 2 or 4 integers
    /// `[offset1 length1]` or `[offset1 length1 offset2 length2]`
    /// (Table F.1, §F.3.3). `/H` is *required*, so a genuine parse always
    /// yields `Some`; an absent, wrong-arity, or non-integer `/H` is
    /// reported as [`LinearizationKind::Malformed`].
    pub hint_offsets: Option<Vec<usize>>,
    /// Value of `/P` — first page number. `None` when absent (treat as 0).
    pub first_page_number: Option<i32>,
}

/// Reason a linearization parameter dict failed to parse into a
/// complete [`Linearization`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinearizationError {
    /// A required key was missing. The byte slice is the key name
    /// (e.g. `b"L"`, `b"O"`, `b"E"`, `b"T"`, `b"N"`).
    MissingRequired(&'static [u8]),
    /// A required key was present but its integer value did not fit in
    /// `usize` on this target.
    OffsetTooLarge(&'static [u8]),
    /// A required key was present but of the wrong type (e.g. `/L` was
    /// a name rather than a number).
    InvalidType(&'static [u8]),
    /// A required array-valued key had the wrong number of elements
    /// (e.g. `/H` was neither 2 nor 4 integers, per Table F.1). The byte
    /// slice is the key name.
    InvalidArity(&'static [u8]),
}

/// State of the linearization parameter dict in a document.
///
/// This enum distinguishes a non-linearized document from a document
/// that *claims* linearization but whose parameter dict is broken — a
/// distinction meaningful for conformance reporting but collapsed by
/// an `Option<Linearization>` API.
///
/// # Three layers of linearization
///
/// Linearization has three progressively stronger properties, each the
/// subset of the one before it:
///
/// 1. **Claimed** — [`Pdf::is_linearized`] is `true`: the first indirect
///    object carries a `/Linearized` key. Holds for both [`Malformed`]
///    and [`Present`]; only [`Absent`] fails it.
/// 2. **Parsed** — [`Pdf::linearization`] is `Some`, i.e. this enum is
///    [`Present`]: every required Table F.1 entry parsed successfully.
/// 3. **Genuine** — [`Pdf::is_genuinely_linearized`] is `true`: the
///    parsed `/L` equals the actual file length (Table F.1, §G.2), so
///    the file may be opened at its first page. See [`Genuineness`].
///
/// This enum is the *parsing* axis (layers 1–2). Genuineness is a
/// separate *runtime* axis (layer 3) and does not affect the variants
/// here: a [`Present`] dict whose `/L` mismatches the file length stays
/// [`Present`].
///
/// [`Pdf::is_linearized`]: crate::Pdf::is_linearized
/// [`Pdf::linearization`]: crate::Pdf::linearization
/// [`Pdf::is_genuinely_linearized`]: crate::Pdf::is_genuinely_linearized
/// [`Absent`]: LinearizationKind::Absent
/// [`Malformed`]: LinearizationKind::Malformed
/// [`Present`]: LinearizationKind::Present
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum LinearizationKind<'a> {
    /// No `/Linearized` key found in the first indirect object.
    Absent,
    /// `/Linearized` is present but the parameter dict failed to parse.
    /// `dict` is the raw dict as it appeared in source so callers can
    /// extract diagnostic information.
    Malformed {
        /// The raw parameter dict.
        dict: Dict<'a>,
        /// The reason parsing failed.
        reason: LinearizationError,
    },
    /// Valid linearization parameter dict.
    Present(Linearization),
}

/// Whether a document's linearization is *genuine* — the runtime
/// validity layer, distinct from the parse-time [`LinearizationKind`].
///
/// ISO 32000-2 Table F.1 makes `/L` decisive: the declared length "shall
/// be exactly equal to the actual length of the PDF file. A mismatch
/// indicates that the file is not linearized and shall be treated as
/// ordinary PDF file, ignoring linearization information." §G.2 uses this
/// same check as the open-time gate, in lieu of seeking to `startxref`.
///
/// This is layer 3 of the model documented on [`LinearizationKind`]: a
/// [`Present`] parameter dict may still be non-genuine — an update was
/// appended after linearization, or the dict is forged — in which case
/// the file must be treated as an ordinary PDF.
///
/// [`Present`]: LinearizationKind::Present
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Genuineness {
    /// No `/Linearized` dict, or it failed to parse
    /// ([`LinearizationKind::Absent`] or [`LinearizationKind::Malformed`]).
    NotLinearized,
    /// The parameter dict parsed, but `/L` does not equal the actual file
    /// length (Table F.1): an update was appended after linearization, or
    /// the dict is not genuine. Treat as an ordinary PDF file.
    LengthMismatch {
        /// The `/L` value declared in the parameter dict.
        declared: u64,
        /// The actual byte length of the PDF source.
        actual: u64,
    },
    /// The parameter dict parsed and its `/L` equals the actual file
    /// length: the file is genuinely linearized.
    Genuine,
}

/// Internal cached form: no lifetime so it can live in `OnceLock`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CachedLinearization {
    Absent,
    Malformed {
        obj_id: ObjectIdentifier,
        reason: LinearizationError,
    },
    Present(Linearization),
}

/// Detect and parse the linearization parameter dict.
///
/// Returns [`CachedLinearization::Absent`] when no linearization is
/// declared. Otherwise attempts the full parse and returns `Present` or
/// `Malformed` with the reason.
pub(crate) fn detect(xref: &XRef) -> CachedLinearization {
    // Linearization dict is the first indirect object in the file — i.e.
    // the `Normal` entry with the smallest byte offset.
    let Some((id, _)) = xref
        .entries()
        .into_iter()
        .filter_map(|(id, entry)| match entry {
            EntryType::Normal { offset } => Some((id, offset)),
            _ => None,
        })
        .min_by_key(|(_, offset)| *offset)
    else {
        return CachedLinearization::Absent;
    };

    let Some(dict) = xref.get::<Dict<'_>>(id) else {
        return CachedLinearization::Absent;
    };

    if !dict.contains_key(b"Linearized") {
        return CachedLinearization::Absent;
    }

    match parse_linearization(&dict) {
        Ok(lin) => CachedLinearization::Present(lin),
        Err(reason) => CachedLinearization::Malformed { obj_id: id, reason },
    }
}

fn get_required_f64(dict: &Dict<'_>, key: &'static [u8]) -> Result<f64, LinearizationError> {
    if !dict.contains_key(key) {
        return Err(LinearizationError::MissingRequired(key));
    }
    dict.get::<f64>(key)
        .ok_or(LinearizationError::InvalidType(key))
}

fn get_required_i64(dict: &Dict<'_>, key: &'static [u8]) -> Result<i64, LinearizationError> {
    if !dict.contains_key(key) {
        return Err(LinearizationError::MissingRequired(key));
    }
    dict.get::<i64>(key)
        .ok_or(LinearizationError::InvalidType(key))
}

fn get_required_usize(dict: &Dict<'_>, key: &'static [u8]) -> Result<usize, LinearizationError> {
    let value = get_required_i64(dict, key)?;
    usize::try_from(value).map_err(|_| LinearizationError::OffsetTooLarge(key))
}

fn get_required_i32(dict: &Dict<'_>, key: &'static [u8]) -> Result<i32, LinearizationError> {
    let value = get_required_i64(dict, key)?;
    i32::try_from(value).map_err(|_| LinearizationError::OffsetTooLarge(key))
}

fn parse_hint_offsets(dict: &Dict<'_>) -> Result<Vec<usize>, LinearizationError> {
    use crate::object::Array;
    // `/H` is Required (Table F.1): an absent key is a malformed dict, not
    // a legitimately hint-free file.
    if !dict.contains_key(b"H") {
        return Err(LinearizationError::MissingRequired(b"H"));
    }
    let array = dict
        .get::<Array<'_>>(b"H")
        .ok_or(LinearizationError::InvalidType(b"H"))?;

    // `raw_iter` yields one token per array slot regardless of element
    // type, so its count is the true arity — unlike `iter::<i64>()`, which
    // stops at the first non-integer and would silently truncate e.g.
    // `[1234 /Foo 56]` to a single element.
    let arity = array.raw_iter().count();
    if arity != 2 && arity != 4 {
        return Err(LinearizationError::InvalidArity(b"H"));
    }

    // Every slot must be an integer. Reading exactly `arity` elements as
    // i64 means a `None` here is a non-integer element, not exhaustion.
    let mut iter = array.flex_iter();
    let mut out: Vec<usize> = Vec::new();
    for _ in 0..arity {
        let value = iter
            .next::<i64>()
            .ok_or(LinearizationError::InvalidType(b"H"))?;
        out.push(usize::try_from(value).map_err(|_| LinearizationError::OffsetTooLarge(b"H"))?);
    }
    Ok(out)
}

fn parse_linearization(dict: &Dict<'_>) -> Result<Linearization, LinearizationError> {
    let version = get_required_f64(dict, b"Linearized")?;
    let length = get_required_usize(dict, b"L")?;
    let first_page_object = get_required_i32(dict, b"O")?;
    let first_page_end_offset = get_required_usize(dict, b"E")?;
    let main_xref_offset = get_required_usize(dict, b"T")?;
    let page_count = get_required_i32(dict, b"N")?;
    // `/H` is required, so a successful parse always carries offsets; the
    // field stays `Option` to keep the public shape stable.
    let hint_offsets = Some(parse_hint_offsets(dict)?);
    let first_page_number = dict.get::<i32>(b"P");

    Ok(Linearization {
        version,
        length,
        first_page_object,
        first_page_end_offset,
        main_xref_offset,
        page_count,
        hint_offsets,
        first_page_number,
    })
}

impl CachedLinearization {
    pub(crate) fn to_kind<'a>(&self, xref: &'a XRef) -> LinearizationKind<'a> {
        match self {
            Self::Absent => LinearizationKind::Absent,
            Self::Malformed { obj_id, reason } => match xref.get::<Dict<'a>>(*obj_id) {
                Some(dict) => LinearizationKind::Malformed {
                    dict,
                    reason: *reason,
                },
                None => LinearizationKind::Absent,
            },
            Self::Present(lin) => LinearizationKind::Present(lin.clone()),
        }
    }

    pub(crate) fn is_linearized(&self) -> bool {
        !matches!(self, Self::Absent)
    }

    pub(crate) fn as_present(&self) -> Option<&Linearization> {
        match self {
            Self::Present(lin) => Some(lin),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Pdf;
    use alloc::format;
    use alloc::vec::Vec;

    fn build_non_linearized() -> Vec<u8> {
        let mut pdf: Vec<u8> = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.7\n");
        let off1 = pdf.len();
        pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        let off2 = pdf.len();
        pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        let xref_pos = pdf.len();
        pdf.extend_from_slice(b"xref\n0 3\n0000000000 65535 f \n");
        pdf.extend_from_slice(format!("{off1:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off2:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(b"trailer\n<< /Size 3 /Root 1 0 R >>\n");
        pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF").as_bytes());
        pdf
    }

    /// Synthesise a minimally-structured "linearized" PDF whose first
    /// indirect object is a linearization parameter dict. The file does
    /// not actually conform to the Annex F network-streaming contract —
    /// only detection and parsing are exercised.
    fn build_linearized_with_dict(dict_body: &str) -> Vec<u8> {
        let mut pdf: Vec<u8> = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.7\n");
        let off_lin = pdf.len();
        pdf.extend_from_slice(format!("1 0 obj\n{dict_body}\nendobj\n").as_bytes());
        let off_catalog = pdf.len();
        pdf.extend_from_slice(b"2 0 obj\n<< /Type /Catalog /Pages 3 0 R >>\nendobj\n");
        let off_pages = pdf.len();
        pdf.extend_from_slice(b"3 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        let xref_pos = pdf.len();
        pdf.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
        pdf.extend_from_slice(format!("{off_lin:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off_catalog:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off_pages:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(b"trailer\n<< /Size 4 /Root 2 0 R >>\n");
        pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF").as_bytes());
        pdf
    }

    #[test]
    fn absent_for_non_linearized_pdf() {
        let pdf = Pdf::new(build_non_linearized()).expect("pdf loads");
        assert!(matches!(
            pdf.linearization_kind(),
            LinearizationKind::Absent
        ));
        assert!(!pdf.is_linearized());
        assert!(pdf.linearization().is_none());
    }

    #[test]
    fn present_for_complete_linearization_dict() {
        let dict = "<< /Linearized 1.0 /L 1024 /H [1234 56] /O 4 /E 999 /N 1 /T 512 /P 1 >>";
        let pdf = Pdf::new(build_linearized_with_dict(dict)).expect("pdf loads");
        assert!(pdf.is_linearized());
        let lin = pdf.linearization().expect("parsed linearization");
        assert_eq!(lin.version, 1.0);
        assert_eq!(lin.length, 1024);
        assert_eq!(lin.first_page_object, 4);
        assert_eq!(lin.first_page_end_offset, 999);
        assert_eq!(lin.main_xref_offset, 512);
        assert_eq!(lin.page_count, 1);
        assert_eq!(lin.first_page_number, Some(1));
        assert_eq!(lin.hint_offsets, Some(alloc::vec![1234_usize, 56]));
        assert!(matches!(
            pdf.linearization_kind(),
            LinearizationKind::Present(_)
        ));
    }

    #[test]
    fn malformed_missing_required_l() {
        let dict = "<< /Linearized 1.0 /H [1234 56] /O 4 /E 999 /N 1 /T 512 >>";
        let pdf = Pdf::new(build_linearized_with_dict(dict)).expect("pdf loads");
        assert!(pdf.is_linearized());
        assert!(pdf.linearization().is_none());
        match pdf.linearization_kind() {
            LinearizationKind::Malformed { reason, .. } => {
                assert_eq!(reason, LinearizationError::MissingRequired(b"L"));
            }
            other => panic!("expected Malformed(MissingRequired(L)), got {other:?}"),
        }
    }

    #[test]
    fn malformed_missing_required_o() {
        let dict = "<< /Linearized 1.0 /L 1024 /H [1234 56] /E 999 /N 1 /T 512 >>";
        let pdf = Pdf::new(build_linearized_with_dict(dict)).expect("pdf loads");
        match pdf.linearization_kind() {
            LinearizationKind::Malformed { reason, .. } => {
                assert_eq!(reason, LinearizationError::MissingRequired(b"O"));
            }
            other => panic!("expected MissingRequired(O), got {other:?}"),
        }
    }

    #[test]
    fn malformed_invalid_type_on_l() {
        let dict = "<< /Linearized 1.0 /L /Foo /O 4 /E 999 /N 1 /T 512 >>";
        let pdf = Pdf::new(build_linearized_with_dict(dict)).expect("pdf loads");
        match pdf.linearization_kind() {
            LinearizationKind::Malformed { reason, .. } => {
                assert_eq!(reason, LinearizationError::InvalidType(b"L"));
            }
            other => panic!("expected InvalidType(L), got {other:?}"),
        }
    }

    #[test]
    fn linearization_kind_preserves_dict_for_malformed() {
        let dict = "<< /Linearized 1.0 /H [1 2] /O 4 /E 9 /N 1 /T 5 >>";
        let pdf = Pdf::new(build_linearized_with_dict(dict)).expect("pdf loads");
        match pdf.linearization_kind() {
            LinearizationKind::Malformed { dict, .. } => {
                // The dict must still be traversable even though parsing failed.
                assert!(dict.contains_key(b"Linearized"));
                assert!(dict.contains_key(b"O"));
                assert!(!dict.contains_key(b"L"));
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn linearization_cache_returns_stable_reference() {
        let dict = "<< /Linearized 1.0 /L 1024 /H [1 2] /O 4 /E 999 /N 1 /T 512 >>";
        let pdf = Pdf::new(build_linearized_with_dict(dict)).expect("pdf loads");
        let a = pdf.linearization().unwrap() as *const _;
        let b = pdf.linearization().unwrap() as *const _;
        assert_eq!(a, b);
    }

    #[test]
    fn tier_b_real_linearized_fixture() {
        let bytes: &[u8] = include_bytes!(
            "../../../beeld-tests/pdfs/custom/andler-optimal-lot-size_linearized.pdf"
        );
        let pdf = Pdf::new(bytes.to_vec()).expect("linearized fixture loads");
        assert!(pdf.is_linearized());
        let lin = pdf.linearization().expect("parsed linearization");
        assert!(lin.version >= 1.0);
        assert!(lin.page_count >= 1);
        assert!(lin.length > 0);
    }

    /// Parse `build_linearized_with_dict(body)` and return the parse
    /// failure reason, panicking if the dict is not `Malformed`.
    fn malformed_reason(body: &str) -> LinearizationError {
        let pdf = Pdf::new(build_linearized_with_dict(body)).expect("pdf loads");
        match pdf.linearization_kind() {
            LinearizationKind::Malformed { reason, .. } => reason,
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    // ----- /H hardening (Ln5): required, arity {2,4}, integers only -----

    #[test]
    fn h_absent_is_missing_required() {
        // /H is Required (Table F.1): an absent key is a malformed dict.
        let dict = "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 1 /T 512 >>";
        assert_eq!(
            malformed_reason(dict),
            LinearizationError::MissingRequired(b"H")
        );
    }

    #[test]
    fn h_wrong_arity_is_invalid_arity() {
        for body in [
            "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 1 /T 512 /H [1] >>",
            "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 1 /T 512 /H [1 2 3] >>",
            "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 1 /T 512 /H [1 2 3 4 5] >>",
            // Arity 3 (raw_iter counts the /Foo slot), so this is rejected as
            // wrong-arity rather than silently truncated to a single [1234].
            "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 1 /T 512 /H [1234 /Foo 56] >>",
        ] {
            assert_eq!(
                malformed_reason(body),
                LinearizationError::InvalidArity(b"H"),
                "body: {body}"
            );
        }
    }

    #[test]
    fn h_non_integer_element_is_invalid_type() {
        // Valid arity (2) but a non-integer element: an error, not a
        // truncated [1234].
        let dict = "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 1 /T 512 /H [1234 /Foo] >>";
        assert_eq!(
            malformed_reason(dict),
            LinearizationError::InvalidType(b"H")
        );
    }

    #[test]
    fn h_negative_is_offset_too_large() {
        let dict = "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 1 /T 512 /H [-1 2] >>";
        assert_eq!(
            malformed_reason(dict),
            LinearizationError::OffsetTooLarge(b"H")
        );
    }

    #[test]
    fn h_non_array_is_invalid_type() {
        let dict = "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 1 /T 512 /H 5 >>";
        assert_eq!(
            malformed_reason(dict),
            LinearizationError::InvalidType(b"H")
        );
    }

    #[test]
    fn h_four_arity_parses() {
        let dict = "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 1 /T 512 /H [100 200 300 400] >>";
        let pdf = Pdf::new(build_linearized_with_dict(dict)).expect("pdf loads");
        let lin = pdf.linearization().expect("parsed linearization");
        assert_eq!(
            lin.hint_offsets,
            Some(alloc::vec![100_usize, 200, 300, 400])
        );
    }

    // ----- OffsetTooLarge on other required integers, and /P wrong-type -----

    #[test]
    fn l_negative_is_offset_too_large() {
        let dict = "<< /Linearized 1.0 /L -1 /O 4 /E 999 /N 1 /T 512 /H [1 2] >>";
        assert_eq!(
            malformed_reason(dict),
            LinearizationError::OffsetTooLarge(b"L")
        );
    }

    #[test]
    fn o_overflowing_i32_is_offset_too_large() {
        // 3_000_000_000 > i32::MAX but fits i64, so /O overflows the i32 narrow.
        let dict = "<< /Linearized 1.0 /L 1024 /O 3000000000 /E 999 /N 1 /T 512 /H [1 2] >>";
        assert_eq!(
            malformed_reason(dict),
            LinearizationError::OffsetTooLarge(b"O")
        );
    }

    #[test]
    fn n_overflowing_i32_is_offset_too_large() {
        let dict = "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 3000000000 /T 512 /H [1 2] >>";
        assert_eq!(
            malformed_reason(dict),
            LinearizationError::OffsetTooLarge(b"N")
        );
    }

    #[test]
    fn p_wrong_type_yields_none_page_number() {
        // /P is Optional (Table F.1, default 0); a wrong-typed value is not a
        // parse failure — the dict is Present with first_page_number == None.
        let dict = "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 1 /T 512 /H [1 2] /P /Foo >>";
        let pdf = Pdf::new(build_linearized_with_dict(dict)).expect("pdf loads");
        let lin = pdf.linearization().expect("parsed despite bad /P");
        assert!(lin.first_page_number.is_none());
    }

    // ----- Genuineness (Ln1): /L == file length is the only demoting check ---

    #[test]
    fn genuine_fixture_is_genuine() {
        let bytes: &[u8] = include_bytes!(
            "../../../beeld-tests/pdfs/custom/andler-optimal-lot-size_linearized.pdf"
        );
        let pdf = Pdf::new(bytes.to_vec()).expect("linearized fixture loads");
        assert!(pdf.is_genuinely_linearized());
        assert_eq!(pdf.linearization_genuineness(), Genuineness::Genuine);
        let lin = pdf.linearization().expect("parsed linearization");
        // /L equals the actual file length (Table F.1, §G.2).
        assert_eq!(lin.length, 99762);
        assert_eq!(lin.length as u64, pdf.data().len());
    }

    #[test]
    fn appended_update_is_length_mismatch() {
        let bytes: &[u8] = include_bytes!(
            "../../../beeld-tests/pdfs/custom/andler-optimal-lot-size_linearized.pdf"
        );
        let mut b = bytes.to_vec();
        b.push(b'\n'); // simulate an appended update: 99762 -> 99763 bytes.
        let pdf = Pdf::new(b).expect("appended fixture still loads");
        // The claim and the parse are unchanged; only genuineness demotes.
        assert!(pdf.is_linearized());
        assert!(!pdf.is_genuinely_linearized());
        assert_eq!(
            pdf.linearization_genuineness(),
            Genuineness::LengthMismatch {
                declared: 99762,
                actual: 99763,
            }
        );
        // A non-genuine file demotes the fast-path accessors (design §6): both
        // gate on genuine linearization, so both yield None even though the
        // dict still parses.
        assert!(pdf.first_page_byte_range().is_none());
        assert!(pdf.hint_tables().is_none());
    }

    #[test]
    fn forged_dict_not_genuine() {
        // A synthetic file of a few hundred bytes whose /L claims 1024.
        let dict = "<< /Linearized 1.0 /L 1024 /O 4 /E 999 /N 1 /T 512 /H [1234 56] >>";
        let pdf = Pdf::new(build_linearized_with_dict(dict)).expect("pdf loads");
        // Parse axis is untouched: the dict is still Present (orthogonality).
        assert!(matches!(
            pdf.linearization_kind(),
            LinearizationKind::Present(_)
        ));
        // Runtime axis rejects it: /L (1024) != actual file length.
        let actual = pdf.data().len();
        assert_ne!(actual, 1024);
        assert_eq!(
            pdf.linearization_genuineness(),
            Genuineness::LengthMismatch {
                declared: 1024,
                actual,
            }
        );
        assert!(!pdf.is_genuinely_linearized());
    }

    // ----- Hint-table decode against the real fixture (empirical pin) -------

    fn andler() -> Pdf {
        let bytes: &[u8] = include_bytes!(
            "../../../beeld-tests/pdfs/custom/andler-optimal-lot-size_linearized.pdf"
        );
        Pdf::new(bytes.to_vec()).expect("linearized fixture loads")
    }

    #[test]
    fn hint_tables_decode_andler() {
        let pdf = andler();
        let tables = pdf.hint_tables().expect("hint tables decode");
        let lin = pdf.linearization().expect("parsed linearization");
        // One page-offset entry per page (/N == 3).
        assert_eq!(
            tables.page_offset.pages.len(),
            usize::try_from(lin.page_count).unwrap(),
        );
        // The first page references no shared objects (F.4.4 / Table F.4 item 3).
        assert!(tables.page_offset.pages[0].shared_refs.is_empty());
        // The shared object table enters every first-page object (F.4.3): the
        // fixture's first page has 26 objects, all in the first-page sequence
        // (total_entry_count == first_page_entry_count), each a 1-object group.
        assert_eq!(tables.shared.header.first_page_entry_count, 26);
        assert_eq!(tables.shared.header.total_entry_count, 26);
        assert_eq!(tables.shared.groups.len(), 26);
    }

    #[test]
    fn hint_page0_matches_lin() {
        let pdf = andler();
        let tables = pdf.hint_tables().expect("hint tables decode");
        let lin = pdf.linearization().expect("parsed linearization");

        // page 0's object number is /O (Table F.4 item 1).
        assert_eq!(
            tables.page_object_number(0, lin),
            Some(lin.first_page_object),
        );
        assert_eq!(lin.first_page_object, 180);

        // page 0's byte range starts at /O's cross-reference offset and ends at
        // /E — cross-checking the F.4.1 adjustment and the page-0 length.
        let range = tables.page_byte_range(0, lin).expect("page 0 range");
        assert_eq!(range.start, 83298); // offset(/O)
        assert_eq!(range.end, lin.first_page_end_offset as u64); // /E
        assert_eq!(range.end, 88106);
    }

    #[test]
    fn hint_offsets_resolve() {
        let pdf = andler();
        let tables = pdf.hint_tables().expect("hint tables decode");
        let lin = pdf.linearization().expect("parsed linearization");

        // Every derived page object number resolves to a Normal cross-reference
        // entry whose byte offset lies within the derived page byte range.
        for page in 0..usize::try_from(lin.page_count).unwrap() {
            let number = tables
                .page_object_number(page, lin)
                .unwrap_or_else(|| panic!("page {page} object number"));
            let range = tables
                .page_byte_range(page, lin)
                .unwrap_or_else(|| panic!("page {page} byte range"));
            match pdf.xref().entry(ObjectIdentifier::new(number, 0)) {
                Some(EntryType::Normal { offset }) => {
                    let offset = offset as u64;
                    assert!(
                        range.contains(&offset),
                        "page {page}: object {number} at {offset} not in {range:?}",
                    );
                }
                other => panic!("page {page}: object {number} not Normal: {other:?}"),
            }
        }
    }

    /// The `andler` fixture's shared object table has an *empty* shared-objects
    /// section (`first_page_entry_count == total_entry_count == 26`), so its
    /// groups decode identically whether the table is read as one column-major
    /// block or two. This fixture has a *non-empty* section
    /// (`first_page_entry_count == 20`, `total_entry_count == 26`): F.4.3's
    /// group entries are a single column-major block over all 26 groups, and
    /// reading them as two independent blocks overruns the table's byte span
    /// and aborts the whole decode to `None`. Pins the one-block transposition
    /// (design §7.7) against a genuine linearizer with a non-empty section.
    #[test]
    fn hint_tables_decode_non_empty_shared_section() {
        let bytes: &[u8] =
            include_bytes!("../../../beeld-tests/pdfs/custom/font_truetype_slow_post_lookup.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("linearized fixture loads");
        assert!(pdf.is_genuinely_linearized()); // /L == file length == 69778
        let tables = pdf.hint_tables().expect("hint tables decode");
        let lin = pdf.linearization().expect("parsed linearization");

        // One page-offset entry per page (/N == 7); page 0 shares nothing.
        assert_eq!(lin.page_count, 7);
        assert_eq!(tables.page_offset.pages.len(), 7);

        // The shared section is genuinely non-empty: 20 first-page groups and 6
        // shared-section groups, all 26 decoded as one column-major block.
        assert_eq!(tables.shared.header.first_page_entry_count, 20);
        assert_eq!(tables.shared.header.total_entry_count, 26);
        assert_eq!(tables.shared.groups.len(), 26);

        // page 0's first object is /O (251); its byte range ends at /E (13923),
        // cross-checking the F.4.1 adjustment and the page-0 length.
        assert_eq!(
            tables.page_object_number(0, lin),
            Some(lin.first_page_object)
        );
        assert_eq!(lin.first_page_object, 251);
        let range0 = tables.page_byte_range(0, lin).expect("page 0 range");
        assert_eq!(range0.end, lin.first_page_end_offset as u64);
        assert_eq!(range0.end, 13923);

        // Every derived page object number resolves to a Normal cross-reference
        // entry whose byte offset lies within the derived page byte range.
        for page in 0..usize::try_from(lin.page_count).unwrap() {
            let number = tables
                .page_object_number(page, lin)
                .unwrap_or_else(|| panic!("page {page} object number"));
            let range = tables
                .page_byte_range(page, lin)
                .unwrap_or_else(|| panic!("page {page} byte range"));
            match pdf.xref().entry(ObjectIdentifier::new(number, 0)) {
                Some(EntryType::Normal { offset }) => {
                    let offset = offset as u64;
                    assert!(
                        range.contains(&offset),
                        "page {page}: object {number} at {offset} not in {range:?}",
                    );
                }
                other => panic!("page {page}: object {number} not Normal: {other:?}"),
            }
        }
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn first_page_trailer_for_linearized_fixture() {
        let bytes: &[u8] = include_bytes!(
            "../../../beeld-tests/pdfs/custom/andler-optimal-lot-size_linearized.pdf"
        );
        let pdf = Pdf::new(bytes.to_vec()).expect("linearized fixture loads");
        let trailer = pdf.xref().first_page_trailer().expect("first-page trailer");
        let size: i32 = trailer.get(b"Size").expect("Size");
        assert!(size > 0);
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn first_page_trailer_some_for_non_linearized() {
        let pdf = Pdf::new(build_non_linearized()).expect("pdf loads");
        assert!(!pdf.is_linearized());
        // A non-linearized file has its only `trailer` before the single
        // `%%EOF`. `first_page_trailer` returns the last `trailer` keyword
        // before the first `%%EOF`, so here it yields the ordinary trailer;
        // the linearized/non-linearized distinction is provided by
        // `Pdf::is_linearized`, not by this method.
        let trailer = pdf.xref().first_page_trailer().expect("ordinary trailer");
        let size: i32 = trailer.get(b"Size").expect("Size");
        assert_eq!(size, 3);
    }

    /// An xref-*stream*-only file has no `trailer` keyword — its first-page
    /// trailer *is* the cross-reference stream object's dictionary
    /// (ISO 32000-2 Annex F.3.4) — so the lexical `trailer`-keyword scan
    /// finds nothing and `first_page_trailer` returns `None`. This pins the
    /// documented limitation (see `XRef::first_page_trailer`). The
    /// `catalog-in-objstm-aes` fixture is such a file: a single `/Type /XRef`
    /// cross-reference stream, no `trailer` keyword anywhere.
    #[cfg(feature = "inspect")]
    #[test]
    fn first_page_trailer_none_for_xref_stream() {
        let bytes: &[u8] =
            include_bytes!("../../../beeld-tests/pdfs/custom/catalog-in-objstm-aes.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("xref-stream fixture loads");
        assert!(pdf.xref().first_page_trailer().is_none());
    }

    /// Build a minimal PDF that carries two xref sections chained via
    /// `/Prev`. The trailing section (pinned by `startxref`) and the
    /// base section advertise distinct `/ID` arrays so that tests can
    /// tell `trailer()` from `base_trailer()`.
    #[cfg(feature = "inspect")]
    fn build_incremental_pdf_with_distinct_ids() -> Vec<u8> {
        let catalog = "<< /Type /Catalog /Pages 2 0 R >>";
        let pages = "<< /Type /Pages /Kids [] /Count 0 >>";

        let mut pdf: Vec<u8> = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.7\n");
        let off1 = pdf.len();
        pdf.extend_from_slice(format!("1 0 obj\n{catalog}\nendobj\n").as_bytes());
        let off2 = pdf.len();
        pdf.extend_from_slice(format!("2 0 obj\n{pages}\nendobj\n").as_bytes());

        // Base xref with /ID = [<AAAA> <BBBB>].
        let xref_a_pos = pdf.len();
        pdf.extend_from_slice(b"xref\n0 3\n");
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        pdf.extend_from_slice(format!("{off1:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off2:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(b"trailer\n<< /Size 3 /Root 1 0 R /ID [<AAAA> <BBBB>] >>\n");
        pdf.extend_from_slice(format!("startxref\n{xref_a_pos}\n%%EOF\n").as_bytes());

        // Incremental update: add obj 3, chained via /Prev to xref_a_pos.
        let off3 = pdf.len();
        pdf.extend_from_slice(b"3 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        let xref_b_pos = pdf.len();
        pdf.extend_from_slice(b"xref\n3 1\n");
        pdf.extend_from_slice(format!("{off3:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size 4 /Root 1 0 R /Prev {xref_a_pos} \
                 /ID [<CCCC> <DDDD>] >>\n"
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(format!("startxref\n{xref_b_pos}\n%%EOF").as_bytes());
        pdf
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn base_trailer_returns_terminus_of_prev_chain() {
        use crate::object::Array;
        use crate::object::String as PdfString;

        let bytes = build_incremental_pdf_with_distinct_ids();
        let pdf = Pdf::new(bytes).expect("pdf loads");

        // trailer() is pinned at startxref (the most-recent update
        // section, with /ID = [<CCCC> <DDDD>]).
        let startxref_trailer = pdf.trailer().expect("startxref trailer");
        let startxref_id_first = startxref_trailer
            .get::<Array<'_>>(b"ID")
            .and_then(|a| a.flex_iter().next::<PdfString<'_>>())
            .expect("update /ID[0]");
        assert_eq!(startxref_id_first.as_ref(), &[0xCC, 0xCC]);

        // base_trailer() walks /Prev to the terminus (the base
        // section, with /ID = [<AAAA> <BBBB>]).
        let base = pdf.base_trailer().expect("base trailer");
        let base_id_first = base
            .get::<Array<'_>>(b"ID")
            .and_then(|a| a.flex_iter().next::<PdfString<'_>>())
            .expect("base /ID[0]");
        assert_eq!(base_id_first.as_ref(), &[0xAA, 0xAA]);
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn base_trailer_equals_trailer_for_single_section() {
        let pdf = Pdf::new(build_non_linearized()).expect("pdf loads");
        let trailer = pdf.trailer().expect("trailer");
        let base = pdf.base_trailer().expect("base trailer");
        // A single-section document has one trailer, so base_trailer() must
        // agree with trailer() on identity and contents — not merely be Some.
        let trailer_size: i32 = trailer.get(b"Size").expect("Size");
        let base_size: i32 = base.get(b"Size").expect("Size");
        assert_eq!(trailer_size, base_size);
        assert_eq!(base_size, 3);
        assert_eq!(
            base.get_ref(b"Root").expect("base /Root ref"),
            trailer.get_ref(b"Root").expect("trailer /Root ref"),
        );
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn base_trailer_for_linearized_fixture_differs_from_trailer() {
        let bytes: &[u8] = include_bytes!(
            "../../../beeld-tests/pdfs/custom/andler-optimal-lot-size_linearized.pdf"
        );
        let pdf = Pdf::new(bytes.to_vec()).expect("linearized fixture loads");
        assert!(pdf.is_linearized());

        // startxref-pinned trailer == first-page trailer for a linearised
        // PDF: startxref gives the offset of the first-page cross-reference
        // table (ISO 32000-2 Annex F.3.11, "Main cross-reference and trailer").
        let startxref_trailer = pdf.trailer().expect("startxref trailer");
        let base = pdf.base_trailer().expect("base trailer");

        // The first-page trailer and the main trailer cover different
        // byte spans, so their raw dict slices must differ.
        assert_ne!(
            startxref_trailer.data().as_ptr(),
            base.data().as_ptr(),
            "linearised file must expose distinct first-page and main trailers"
        );
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn base_trailer_on_dummy_xref_is_none() {
        use crate::xref::XRef;
        let dummy = XRef::dummy();
        assert!(dummy.base_trailer().is_none());
    }

    /// The `catalog-in-objstm-aes` fixture is AES-256 encrypted and has a
    /// single cross-reference *stream* section whose dict carries a direct
    /// `/ID`. Per ISO 32000-1 §7.5.8.2 the cross-reference stream
    /// dictionary's strings shall not be encrypted, so `base_trailer()`
    /// (which walks to the terminal Stream section) must return the same raw
    /// `/ID` as `trailer()` — never a value run through the cipher. Before
    /// the fix, the Stream path read the dict as an indirect object, which
    /// set the object number and decrypted its strings.
    #[cfg(feature = "inspect")]
    #[test]
    fn base_trailer_does_not_decrypt_xref_stream_dict_strings() {
        use crate::object::Array;
        use crate::object::String as PdfString;

        let bytes: &[u8] =
            include_bytes!("../../../beeld-tests/pdfs/custom/catalog-in-objstm-aes.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("xref-stream aes fixture loads");
        assert!(pdf.is_encrypted());

        let id_first = |d: &Dict<'_>| -> Vec<u8> {
            d.get::<Array<'_>>(b"ID")
                .and_then(|a| a.flex_iter().next::<PdfString<'_>>())
                .expect("/ID[0]")
                .to_vec()
        };

        let base = pdf.base_trailer().expect("base trailer");
        let trailer = pdf.trailer().expect("trailer");
        let base_id = id_first(&base);
        let trailer_id = id_first(&trailer);

        // trailer() is known not to decrypt the trailer dict; base_trailer()
        // must agree, and the raw /ID here is exactly 16 bytes.
        assert_eq!(
            base_id.len(),
            16,
            "xref-stream dict /ID must be raw 16 bytes"
        );
        assert_eq!(
            base_id, trailer_id,
            "base_trailer() must not decrypt the cross-reference stream dict"
        );
    }
}
