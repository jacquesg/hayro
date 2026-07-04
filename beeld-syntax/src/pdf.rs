//! The starting point for reading PDF files.

use crate::PdfData;
use crate::ReadAt;
use crate::ReadAtError;
use crate::data::FULL_FALLBACK_MAX;
use crate::object::{Dict, Object};
use crate::page::Pages;
use crate::page::cached::CachedPages;
use crate::reader::Reader;
use crate::sync::Arc;
use crate::xref::{XRef, XRefError, fallback, root_xref, root_xref_streamed};

pub use crate::crypto::DecryptionError;
use crate::metadata::Metadata;
use alloc::vec::Vec;

/// A PDF file.
pub struct Pdf {
    xref: Arc<XRef>,
    header_version: PdfVersion,
    pages: CachedPages,
    data: PdfData,
    #[cfg(feature = "inspect")]
    layout: crate::sync::OnceLock<crate::layout::FileLayout>,
    linearization: crate::sync::OnceLock<crate::linearization::CachedLinearization>,
}

/// An error that occurred while loading a PDF file.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LoadPdfError {
    /// An error occurred while processing an encrypted document.
    Decryption(DecryptionError),
    /// The PDF was invalid or could not be parsed due to some other unknown reason.
    Invalid,
}

#[allow(clippy::len_without_is_empty)]
impl Pdf {
    /// Try to read the given PDF file.
    ///
    /// Returns `Err` if it was unable to read it.
    pub fn new(data: impl Into<PdfData>) -> Result<Self, LoadPdfError> {
        Self::new_with_password(data, "")
    }

    /// Try to read the given PDF file with a password.
    ///
    /// Returns `Err` if it was unable to read it or if the password is incorrect.
    pub fn new_with_password(
        data: impl Into<PdfData>,
        password: &str,
    ) -> Result<Self, LoadPdfError> {
        let data = data.into();

        // Streaming source: try a bounded open that reads only what it needs.
        // Multi-section (`/Prev`) and hybrid (`/XRefStm`) xrefs stream
        // natively; fall back to a resident parse (materialise) only when the
        // xref cannot be followed by bounded reads (a section larger than
        // `STREAM_SECTION_MAX`, a circular or too-deep `/Prev` chain, an
        // overflowing subsection header, or an unresolvable trailer), so
        // `get_with` uses the resident path and never double-reads.
        if data.is_streamed() {
            if let Some(pdf) = Self::try_open_streamed(&data, password) {
                return Ok(pdf);
            }
            return Self::new_with_password(
                PdfData::from(
                    materialise_streamed(&data, FULL_FALLBACK_MAX)
                        .map_err(|_| LoadPdfError::Invalid)?,
                ),
                password,
            );
        }

        let password = password.as_bytes();
        let version = find_version(data.as_ref()).unwrap_or(PdfVersion::Pdf10);
        let xref = match root_xref(data.clone(), password) {
            Ok(x) => x,
            Err(e) => match e {
                XRefError::Unknown => {
                    fallback(data.clone(), password).ok_or(LoadPdfError::Invalid)?
                }
                XRefError::Encryption(e) => return Err(LoadPdfError::Decryption(e)),
            },
        };
        let xref = Arc::new(xref);

        let pages = CachedPages::new(xref.clone()).ok_or(LoadPdfError::Invalid)?;

        Ok(Self {
            xref,
            header_version: version,
            pages,
            data,
            #[cfg(feature = "inspect")]
            layout: crate::sync::OnceLock::new(),
            linearization: crate::sync::OnceLock::new(),
        })
    }

    /// Try to read a PDF from a positioned-read [`ReadAt`] source, parsing
    /// it on demand instead of from a fully-resident buffer.
    ///
    /// PDF is a random-access format, so the trailer and cross-reference
    /// table are read with bounded reads and each accessed object's byte
    /// range is fetched as needed — a large document does not have to be
    /// held wholly in memory. A malformed file whose xref must be rebuilt by
    /// brute force falls back to a full read (see [`crate::ReadAt`]).
    ///
    /// Returns `Err` if it was unable to read it.
    #[cfg(feature = "std")]
    pub fn new_with_reader<S: ReadAt + Send + Sync + 'static>(
        source: S,
    ) -> Result<Self, LoadPdfError> {
        Self::new_with_password(PdfData::streamed(source), "")
    }

    /// Try to read a PDF from a positioned-read [`ReadAt`] source.
    ///
    /// Returns `Err` if it was unable to read it.
    #[cfg(not(feature = "std"))]
    pub fn new_with_reader<S: ReadAt + 'static>(source: S) -> Result<Self, LoadPdfError> {
        Self::new_with_password(PdfData::streamed(source), "")
    }

    /// Try to read a password-protected PDF from a positioned-read
    /// [`ReadAt`] source.
    ///
    /// Returns `Err` if it was unable to read it or if the password is
    /// incorrect.
    #[cfg(feature = "std")]
    pub fn new_with_reader_and_password<S: ReadAt + Send + Sync + 'static>(
        source: S,
        password: &str,
    ) -> Result<Self, LoadPdfError> {
        Self::new_with_password(PdfData::streamed(source), password)
    }

    /// Try to read a password-protected PDF from a positioned-read
    /// [`ReadAt`] source.
    #[cfg(not(feature = "std"))]
    pub fn new_with_reader_and_password<S: ReadAt + 'static>(
        source: S,
        password: &str,
    ) -> Result<Self, LoadPdfError> {
        Self::new_with_password(PdfData::streamed(source), password)
    }

    /// Bounded streaming open: parse the version, xref table and page tree
    /// from positioned reads of `data` without materialising the whole file.
    /// Multi-section (`/Prev`) and hybrid (`/XRefStm`) xrefs stream natively;
    /// returns `None` only when the xref cannot be followed by bounded reads
    /// (a section larger than `STREAM_SECTION_MAX`, a circular or too-deep
    /// `/Prev` chain, an overflowing subsection header, or an unresolvable
    /// trailer), so the caller can fall back to a resident parse.
    fn try_open_streamed(data: &PdfData, password: &str) -> Option<Self> {
        let file_len = data.len();
        let head_len = core::cmp::min(
            VERSION_SCAN_MAX,
            usize::try_from(file_len).unwrap_or(usize::MAX),
        );
        let head = data.read_range(0, head_len);
        let version = find_version(&head).unwrap_or(PdfVersion::Pdf10);

        let xref = root_xref_streamed(data.clone(), password.as_bytes(), file_len).ok()?;
        let xref = Arc::new(xref);
        let pages = CachedPages::new(xref.clone())?;

        Some(Self {
            xref,
            header_version: version,
            pages,
            data: data.clone(),
            #[cfg(feature = "inspect")]
            layout: crate::sync::OnceLock::new(),
            linearization: crate::sync::OnceLock::new(),
        })
    }

    /// Return the number of objects present in the PDF file.
    pub fn len(&self) -> usize {
        self.xref.len()
    }

    /// Return an iterator over all objects defined in the PDF file.
    pub fn objects(&self) -> impl IntoIterator<Item = Object<'_>> {
        self.xref.objects()
    }

    /// Return the version of the PDF file.
    pub fn version(&self) -> PdfVersion {
        self.xref
            .trailer_data()
            .version
            .unwrap_or(self.header_version)
    }

    /// Return the underlying data of the PDF file.
    pub fn data(&self) -> &PdfData {
        &self.data
    }

    /// Return the pages of the PDF file.
    pub fn pages(&self) -> &Pages<'_> {
        self.pages.get()
    }

    /// Return the xref of the PDF file.
    pub fn xref(&self) -> &XRef {
        &self.xref
    }

    /// Return the metadata in the document information dictionary of the document.
    pub fn metadata(&self) -> &Metadata {
        self.xref.metadata()
    }

    /// Return the document's trailer dictionary.
    ///
    /// Convenience accessor for [`XRef::trailer`]. See that method for
    /// behaviour and the re-parse performance note.
    pub fn trailer(&self) -> Option<Dict<'_>> {
        self.xref.trailer()
    }

    /// Return the trailer of the base (oldest) cross-reference section —
    /// the `/Prev`-chain terminus.
    ///
    /// Convenience accessor for [`XRef::base_trailer`]. See that
    /// method for semantics — in particular, for a linearised document
    /// this differs from [`Self::trailer`] because `startxref` points
    /// at the first-page xref section while `/Prev` reaches the main
    /// xref at the tail.
    ///
    /// Requires the `inspect` feature.
    #[cfg(feature = "inspect")]
    pub fn base_trailer(&self) -> Option<Dict<'_>> {
        self.xref.base_trailer()
    }

    /// Return the state of the document's `/Encrypt` trailer entry.
    ///
    /// Convenience accessor for [`XRef::encryption_kind`]. Use this to tell
    /// a plaintext document (no `/Encrypt`) from one whose `/Encrypt` entry
    /// is present but malformed; most callers can use the convenience
    /// [`Self::encryption_dict`] / [`Self::is_encrypted`].
    pub fn encryption_kind(&self) -> crate::xref::EncryptionKind<'_> {
        self.xref.encryption_kind()
    }

    /// Return the document's encryption dictionary, if any.
    ///
    /// Convenience accessor for [`XRef::encryption_dict`]. See that method
    /// for behaviour and the re-parse performance note. To distinguish an
    /// absent `/Encrypt` from a present-but-malformed one, use
    /// [`Self::encryption_kind`].
    pub fn encryption_dict(&self) -> Option<Dict<'_>> {
        self.xref.encryption_dict()
    }

    /// Whether the document is encrypted.
    ///
    /// Convenience accessor for [`XRef::is_encrypted`].
    pub fn is_encrypted(&self) -> bool {
        self.xref.is_encrypted()
    }

    fn linearization_cached(&self) -> &crate::linearization::CachedLinearization {
        self.linearization
            .get_or_init(|| crate::linearization::detect(&self.xref))
    }

    /// Return the full state of the linearization parameter dictionary
    /// (ISO 32000-1 Annex F).
    ///
    /// Use this when you need to distinguish an absent dict from a
    /// malformed one; most callers can use the convenience
    /// [`Self::linearization`] instead.
    pub fn linearization_kind(&self) -> crate::linearization::LinearizationKind<'_> {
        self.linearization_cached().to_kind(&self.xref)
    }

    /// Return the successfully-parsed linearization parameter dict.
    ///
    /// Returns `None` when the document is not linearized OR when its
    /// linearization dict is malformed. Callers that need to tell the
    /// two cases apart must use [`Self::linearization_kind`].
    pub fn linearization(&self) -> Option<&crate::linearization::Linearization> {
        self.linearization_cached().as_present()
    }

    /// Whether the document claims linearization.
    ///
    /// Returns `true` when the first indirect object has a
    /// `/Linearized` key, regardless of whether the full parameter dict
    /// parses successfully.
    pub fn is_linearized(&self) -> bool {
        self.linearization_cached().is_linearized()
    }

    /// Compute the file-level physical layout.
    ///
    /// Scans the raw bytes on first call; the result is cached and
    /// returned by reference on subsequent calls. Idempotent.
    ///
    /// Requires the `inspect` feature.
    #[cfg(feature = "inspect")]
    pub fn file_layout(&self) -> &crate::layout::FileLayout {
        self.layout
            .get_or_init(|| crate::layout::FileLayout::compute(self.data.as_ref()))
    }
}

/// Bytes scanned from the file head for the `%PDF-` version marker. The
/// streamed open ([`Pdf::try_open_streamed`]) reads exactly this many bytes and
/// [`find_version`] caps its scan at the same bound, so the streamed and
/// resident paths agree on how far the header may sit into the file.
const VERSION_SCAN_MAX: usize = 2000;

fn find_version(data: &[u8]) -> Option<PdfVersion> {
    let data = &data[..data.len().min(VERSION_SCAN_MAX)];
    let mut r = Reader::new(data);

    while r.forward_tag(b"%PDF-").is_none() {
        r.read_byte()?;
    }

    PdfVersion::from_bytes(r.tail()?)
}

/// Materialise a streamed source into a resident buffer for the resident
/// fallback parse, reading in bounded chunks and reserving exactly (never
/// amortised doubling) so the buffer's capacity tracks the bytes actually served
/// rather than the source's untrusted [`ReadAt::len`]. A source reporting a huge
/// or corrupt length therefore cannot force a giant eager allocation (a
/// `handle_alloc_error` abort or a capacity-overflow panic) - it yields only the
/// bytes it has, which the resident parse then accepts or rejects. A legitimate
/// file materialises as the buffer grows with the bytes actually read rather than
/// being pre-sized from the claimed length.
///
/// `cap` bounds the running total independently of the source's end-of-input
/// signal; production passes the whole-file cap [`FULL_FALLBACK_MAX`]. Normal
/// termination is the first empty *successful* read, so a source that signals
/// end-of-input (a short / zero read, per the [`ReadAt`] contract) is
/// materialised in full up to `cap`. A genuine read failure is returned as
/// `Err`, never mistaken for that end-of-input, so a transient mid-stream error
/// cannot silently truncate the buffer. A buggy or adversarial source that never
/// signals end-of-input cannot grow the buffer without bound: once the
/// accumulated total exceeds `cap` the load fails (`Err` ->
/// `LoadPdfError::Invalid`) rather than growing further. Refusing such a source
/// past the whole-file cap is consistent - a file that large could not use the
/// streamed whole-file fallback (`full_bytes`) either - and this resident
/// materialisation exists only to re-parse a *terminating* source whose xref the
/// streamed open could not follow.
fn materialise_streamed(data: &PdfData, cap: u64) -> Result<Vec<u8>, ReadAtError> {
    // A bounded per-read allocation, independent of the claimed length.
    const CHUNK: usize = 1 << 20; // 1 MiB.
    let mut buf = Vec::new();
    let mut offset = 0_u64;
    loop {
        // Read with the checked API so a genuine source failure is distinguished
        // from a real end-of-input: only an `Ok` short / zero read (EOF, per the
        // `ReadAt` contract) terminates the loop; an `Err` propagates. Using
        // `read_range` (`try_read_range(..).unwrap_or_default()`) would collapse
        // the error to an empty `Vec`, be mistaken for EOF here, and return the
        // silently TRUNCATED prefix accumulated so far - which the resident
        // re-parse would then brute-force-repair into a partial document.
        let chunk = data.try_read_range(offset, CHUNK)?;
        if chunk.is_empty() {
            break;
        }
        offset = offset.saturating_add(chunk.len() as u64);
        // Hard-cap the running total BEFORE growing `buf`: a source that never
        // signals end-of-input (violating the `ReadAt` EOF contract by returning
        // non-empty reads past its own length) would otherwise grow `buf` without
        // bound - an allocation abort / OOM. Checking *after* the extend would let
        // the over-cap chunk grow the buffer first, and `extend_from_slice`
        // reserves by amortised doubling: a `buf` already at `cap` would double
        // its reserved capacity toward `2 * cap` (a 4 GiB reservation at the 2 GiB
        // production cap, or a capacity-overflow panic on a 32-bit / wasm32
        // target) before the check fired. Refuse once the next chunk would exceed
        // `cap`, and grow with `try_reserve_exact` (exact, no doubling; `Err`
        // rather than an abort on allocation failure), so reserved capacity never
        // exceeds `cap` and the peak allocation stays at `cap + CHUNK` (the buffer
        // plus the transient chunk).
        if buf.len() as u64 + chunk.len() as u64 > cap {
            return Err(ReadAtError::Io);
        }
        buf.try_reserve_exact(chunk.len())
            .map_err(|_| ReadAtError::Io)?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// The version of a PDF document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PdfVersion {
    /// PDF 1.0.
    Pdf10,
    /// PDF 1.1.
    Pdf11,
    /// PDF 1.2.
    Pdf12,
    /// PDF 1.3.
    Pdf13,
    /// PDF 1.4.
    Pdf14,
    /// PDF 1.5.
    Pdf15,
    /// PDF 1.6.
    Pdf16,
    /// PDF 1.7.
    Pdf17,
    /// PDF 2.0.
    Pdf20,
}

impl PdfVersion {
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        match bytes.get(..3)? {
            b"1.0" => Some(Self::Pdf10),
            b"1.1" => Some(Self::Pdf11),
            b"1.2" => Some(Self::Pdf12),
            b"1.3" => Some(Self::Pdf13),
            b"1.4" => Some(Self::Pdf14),
            b"1.5" => Some(Self::Pdf15),
            b"1.6" => Some(Self::Pdf16),
            b"1.7" => Some(Self::Pdf17),
            b"2.0" => Some(Self::Pdf20),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::pdf::{Pdf, PdfVersion};

    #[test]
    fn issue_49() {
        let _ = Pdf::new(Vec::new());
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn file_layout_matches_real_fixture() {
        let bytes: &[u8] =
            include_bytes!("../../beeld-tests/pdfs/custom/andler-optimal-lot-size.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("fixture loads");
        let layout = pdf.file_layout();
        assert!(!layout.eof_offsets.is_empty());
        assert_eq!(layout.file_size, bytes.len());
        // Header must be within the file.
        assert!(layout.header_offset < layout.file_size);
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn file_layout_is_cached() {
        let bytes: &[u8] =
            include_bytes!("../../beeld-tests/pdfs/custom/andler-optimal-lot-size.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("fixture loads");
        let first = pdf.file_layout() as *const _;
        let second = pdf.file_layout() as *const _;
        // Cache returns the same reference, not a fresh computation.
        assert_eq!(first, second);
    }

    #[test]
    #[ignore = "vendored copy omits beeld-tests/downloads/ — restore the upstream beeld-tests \
                submodule to run this fixture; the mangwhap vendor of beeld-syntax \
                ships only the `pdfs/custom/` fixtures referenced by `include_bytes!`"]
    fn pdf_version_header() {
        let data = std::fs::read("../beeld-tests/downloads/pdfjs/alphatrans.pdf").unwrap();
        let pdf = Pdf::new(data).unwrap();

        assert_eq!(pdf.version(), PdfVersion::Pdf17);
    }

    #[test]
    #[ignore = "vendored copy omits beeld-tests/downloads/ — see pdf_version_header"]
    fn pdf_version_catalog() {
        let data = std::fs::read("../beeld-tests/downloads/pdfbox/2163.pdf").unwrap();
        let pdf = Pdf::new(data).unwrap();

        assert_eq!(pdf.version(), PdfVersion::Pdf14);
    }

    // --- streaming (ReadAt) ingress ---

    /// A [`crate::ReadAt`] source over an in-memory buffer that counts the
    /// bytes it serves, so a test can assert how much of the file the
    /// streaming parse actually touched.
    struct CountingReadAt {
        data: Vec<u8>,
        bytes_read: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl crate::ReadAt for CountingReadAt {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, crate::ReadAtError> {
            let start = (offset as usize).min(self.data.len());
            let avail = &self.data[start..];
            let n = avail.len().min(buf.len());
            buf[..n].copy_from_slice(&avail[..n]);
            self.bytes_read
                .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            Ok(n)
        }
    }

    fn page_op_counts(pdf: &Pdf) -> Vec<usize> {
        pdf.pages()
            .iter()
            .map(|page| {
                let mut ops = page.typed_operations();
                let mut n = 0;
                while ops.next().is_some() {
                    n += 1;
                }
                n
            })
            .collect()
    }

    /// A document parsed from a [`crate::ReadAt`] source must yield exactly
    /// the same pages and content-stream operators as the same bytes parsed
    /// from a resident buffer.
    #[test]
    fn streamed_parse_matches_resident() {
        let bytes: &[u8] =
            include_bytes!("../../beeld-tests/pdfs/custom/andler-optimal-lot-size.pdf");

        let resident = Pdf::new(bytes.to_vec()).expect("resident fixture loads");

        let bytes_read = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let source = CountingReadAt {
            data: bytes.to_vec(),
            bytes_read: bytes_read.clone(),
        };
        let streamed = Pdf::new_with_reader(source).expect("streamed fixture loads");

        let resident_ops = page_op_counts(&resident);
        let streamed_ops = page_op_counts(&streamed);
        assert!(
            !streamed_ops.is_empty(),
            "fixture must have at least one page"
        );
        assert_eq!(
            resident_ops, streamed_ops,
            "streamed parse must match resident parse operator-for-operator",
        );
    }

    /// Opening a streamed document and walking its page tree must read far
    /// less than the whole file — the page content streams are not touched, so
    /// streaming genuinely avoids buffering the entire file.
    #[test]
    fn streamed_open_reads_less_than_whole_file() {
        let bytes: &[u8] =
            include_bytes!("../../beeld-tests/pdfs/custom/andler-optimal-lot-size.pdf");

        let bytes_read = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let source = CountingReadAt {
            data: bytes.to_vec(),
            bytes_read: bytes_read.clone(),
        };
        let pdf = Pdf::new_with_reader(source).expect("streamed fixture loads");

        // Touch the page tree (catalog -> pages -> page dicts), but NOT the
        // page content streams.
        let page_count = pdf.pages().iter().count();
        assert!(page_count >= 1, "fixture must have at least one page");

        let read = bytes_read.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            read < bytes.len() as u64,
            "streaming open read {read} of {} bytes - expected a bounded read",
            bytes.len(),
        );
    }

    /// A linearized document has a multi-section cross-reference (the main
    /// table plus a `/Prev`-chained first-page table). It must stream too -
    /// parse identically to the resident path AND, when only the page tree is
    /// walked, read less than the whole file - rather than falling back to a
    /// resident parse.
    #[test]
    fn streamed_multi_section_xref_streams() {
        let bytes: &[u8] =
            include_bytes!("../../beeld-tests/pdfs/custom/andler-optimal-lot-size_linearized.pdf");

        let resident = Pdf::new(bytes.to_vec()).expect("resident fixture loads");
        let streamed = Pdf::new_with_reader(CountingReadAt {
            data: bytes.to_vec(),
            bytes_read: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
        .expect("streamed fixture loads");
        assert_eq!(
            page_op_counts(&resident),
            page_op_counts(&streamed),
            "streamed multi-section parse must match resident parse",
        );

        let bytes_read = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pdf = Pdf::new_with_reader(CountingReadAt {
            data: bytes.to_vec(),
            bytes_read: bytes_read.clone(),
        })
        .expect("streamed fixture loads");
        assert!(pdf.pages().iter().count() >= 1);
        let read = bytes_read.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            read < bytes.len() as u64,
            "multi-section streaming open read {read} of {} bytes - expected bounded reads, not a resident fallback",
            bytes.len(),
        );
    }

    /// A hybrid-reference file (ISO 32000-1 §7.5.8.4: a classic table plus a
    /// `/XRefStm` cross-reference stream) parsed from a [`crate::ReadAt`]
    /// source must yield exactly the same pages and operators as the resident
    /// parse.
    #[test]
    fn streamed_hybrid_xref_matches_resident() {
        let bytes: &[u8] =
            include_bytes!("../../beeld-tests/pdfs/custom/font_truetype_slow_post_lookup.pdf");

        let resident = Pdf::new(bytes.to_vec()).expect("resident hybrid loads");
        let streamed = Pdf::new_with_reader(CountingReadAt {
            data: bytes.to_vec(),
            bytes_read: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
        .expect("streamed hybrid loads");

        assert_eq!(
            page_op_counts(&resident),
            page_op_counts(&streamed),
            "streamed hybrid-reference parse must match resident parse",
        );
    }

    /// A positioned-read source over an owned buffer.
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

    /// Build a minimal single-page PDF with a classic cross-reference table.
    /// Object 4 is a standalone dictionary (`<< /Marker 4444 >>`) the page tree
    /// never references, so it is resolved only on an explicit lookup.
    /// `obj4_xref_offset` overrides the xref entry for object 4, letting a test
    /// point it inside object 4's own extent to force a repair.
    fn minimal_pdf(obj4_xref_offset: Option<usize>) -> Vec<u8> {
        let obj1: &[u8] = b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
        let obj2: &[u8] = b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n";
        let obj3: &[u8] =
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>\nendobj\n";
        let obj4: &[u8] = b"4 0 obj\n<< /Marker 4444 >>\nendobj\n";

        let mut body = Vec::new();
        body.extend_from_slice(b"%PDF-1.7\n");
        let off1 = body.len();
        body.extend_from_slice(obj1);
        let off2 = body.len();
        body.extend_from_slice(obj2);
        let off3 = body.len();
        body.extend_from_slice(obj3);
        let off4 = body.len();
        body.extend_from_slice(obj4);
        let xoff = body.len();

        let e4 = obj4_xref_offset.unwrap_or(off4);
        let xref = format!(
            "xref\n0 5\n\
             0000000000 65535 f \n\
             {off1:010} 00000 n \n\
             {off2:010} 00000 n \n\
             {off3:010} 00000 n \n\
             {e4:010} 00000 n \n\
             trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xoff}\n%%EOF\n",
        );
        body.extend_from_slice(xref.as_bytes());
        body
    }

    fn oid(obj: i32) -> crate::object::ObjectIdentifier {
        crate::object::ObjectIdentifier::new(obj, 0)
    }

    /// S2 (repair reroute): after a whole-file repair, a streamed lookup must
    /// find the object at its rebuilt offset. Object 4's xref entry is pointed a
    /// few bytes into its own body, so the streamed window read starts
    /// mid-object and fails, forcing a repair. The stale sorted-offset (the
    /// corrupt value) then truncates the repaired object's streamed window
    /// unless the lookup is rerouted to the repaired/resident table.
    #[test]
    fn streamed_repair_then_lookup_finds_object() {
        let clean = minimal_pdf(None);
        let off4 =
            crate::util::find_needle(&clean, b"4 0 obj").expect("object 4 present in fixture");

        // Sanity: the clean document streams and resolves object 4 with no repair.
        let clean_pdf = Pdf::new_with_reader(SliceReadAt(clean)).expect("clean streamed load");
        assert!(
            clean_pdf.data().is_streamed(),
            "the minimal fixture must take the streamed open path",
        );
        assert_eq!(
            clean_pdf
                .xref()
                .get::<crate::object::Dict<'_>>(oid(4))
                .and_then(|d| d.get::<i32>(b"Marker")),
            Some(4444),
        );

        // Corrupt object 4's offset to land inside its own body.
        let corrupt = minimal_pdf(Some(off4 + 4));
        let pdf =
            Pdf::new_with_reader(SliceReadAt(corrupt)).expect("corrupt streamed open succeeds");
        let marker = pdf
            .xref()
            .get::<crate::object::Dict<'_>>(oid(4))
            .and_then(|d| d.get::<i32>(b"Marker"));
        assert_eq!(
            marker,
            Some(4444),
            "the repaired object must be found via the rerouted lookup, not lost",
        );
    }

    /// S6 (contended repair): many threads racing into the whole-file repair
    /// path must not panic - the old `try_put().unwrap()` / `assert!(!repaired)`
    /// did under contention - nor deadlock, and every one must still find the
    /// repaired object.
    #[test]
    fn concurrent_repair_is_panic_free() {
        let clean = minimal_pdf(None);
        let off4 = crate::util::find_needle(&clean, b"4 0 obj").expect("object 4 present");
        let corrupt = minimal_pdf(Some(off4 + 4));
        let pdf = std::sync::Arc::new(
            Pdf::new_with_reader(SliceReadAt(corrupt)).expect("corrupt streamed open succeeds"),
        );

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let pdf = pdf.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    pdf.xref()
                        .get::<crate::object::Dict<'_>>(oid(4))
                        .and_then(|d| d.get::<i32>(b"Marker"))
                })
            })
            .collect();
        for h in handles {
            assert_eq!(
                h.join()
                    .expect("worker thread panicked - concurrent repair was not panic-free"),
                Some(4444),
                "every racing lookup must find the repaired object",
            );
        }
    }

    /// A positioned-read source that records the largest `len` any single
    /// `read_range` requests, WITHOUT eagerly allocating it. A window sized past
    /// the file (the sentinel-ordering `DoS`) would otherwise force a giant
    /// `vec![0; len]`; overriding `read_range` here means such a request only
    /// inflates the recorded maximum, so the test observes it rather than
    /// OOM-ing on it.
    struct RecordingReadAt {
        data: Vec<u8>,
        max_len: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl crate::ReadAt for RecordingReadAt {
        fn len(&self) -> u64 {
            self.data.len() as u64
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, crate::ReadAtError> {
            let start = (offset as usize).min(self.data.len());
            let avail = &self.data[start..];
            let n = avail.len().min(buf.len());
            buf[..n].copy_from_slice(&avail[..n]);
            Ok(n)
        }

        fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>, crate::ReadAtError> {
            self.max_len
                .fetch_max(len as u64, std::sync::atomic::Ordering::Relaxed);
            let start = (offset as usize).min(self.data.len());
            let avail = &self.data[start..];
            let n = avail.len().min(len);
            Ok(avail[..n].to_vec())
        }
    }

    /// A live object's on-demand window must never be sized past the file. A
    /// decoy in-use xref entry with the highest offset (well beyond EOF) must not
    /// blow up the window of the largest real object below it: the file-length
    /// sentinel is sorted INTO the offset index, not appended after it, so every
    /// window is bounded by the real file length. Regressing the sentinel
    /// ordering sizes that window at the decoy offset - a multi-GiB on-demand
    /// read (memory-amplification `DoS`).
    #[test]
    fn streamed_object_window_bounded_by_file_length() {
        // Object 4's xref entry points ~4 GiB past EOF; object 3 (the page) is
        // the largest real offset below it and is resolved while opening.
        let bytes = minimal_pdf(Some(4_000_000_000));
        let file_len = bytes.len() as u64;
        let max_len = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pdf = Pdf::new_with_reader(RecordingReadAt {
            data: bytes,
            max_len: max_len.clone(),
        })
        .expect("streamed open with an out-of-range decoy entry still succeeds");
        // Walk the page tree (touches object 3's window).
        let _ = pdf.pages().iter().count();

        let observed = max_len.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            observed <= file_len,
            "no on-demand read may exceed the {file_len}-byte file; a window sized at the \
             out-of-range decoy offset requested {observed} bytes (sentinel-ordering DoS)",
        );
    }

    /// A streamed source that reports an oversize length and fails the bounded
    /// streamed open must fall back WITHOUT allocating the claimed length:
    /// `Pdf::new_with_reader` returns `Err(Invalid)` (bounded) rather than
    /// aborting on a `vec![0; usize::MAX]` fabricated from the untrusted
    /// `ReadAt::len`. Exercises the public streamed->resident fallback that the
    /// capped-`as_ref` unit test `whole_file_fallback_refuses_oversize_length`
    /// does not reach.
    #[test]
    fn streamed_open_oversize_len_falls_back_without_giant_alloc() {
        struct HugeLenSource;

        impl crate::ReadAt for HugeLenSource {
            fn len(&self) -> u64 {
                u64::MAX
            }

            fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, crate::ReadAtError> {
                Ok(0)
            }
        }

        match Pdf::new_with_reader(HugeLenSource) {
            Err(super::LoadPdfError::Invalid) => {}
            Err(other) => panic!("expected Err(Invalid), got Err({other:?})"),
            Ok(_) => panic!("expected Err(Invalid); an oversize len was not bounded"),
        }
    }

    /// `materialise_streamed` must accumulate a multi-`CHUNK` source to
    /// completion and stop at end-of-input. The oversize fixtures return
    /// `Ok(0)` on the first read, so the multi-iteration accumulation + EOF
    /// termination - the behaviour the loop's doc states it relies on - was
    /// otherwise unexercised. (A source that never signals EOF is out of scope
    /// by that contract: it cannot be pinned without unbounded growth.)
    #[test]
    fn materialise_streamed_reads_multi_chunk_source_to_eof() {
        // 2.5 MiB: larger than one 1 MiB CHUNK and not a chunk multiple, so the
        // loop spans several reads and the final read is short before EOF.
        const LEN: usize = (2 << 20) + (1 << 19);
        let mut data = vec![0_u8; LEN];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let materialised = super::materialise_streamed(
            &crate::PdfData::streamed(SliceReadAt(data.clone())),
            crate::data::FULL_FALLBACK_MAX,
        )
        .expect("a fault-free multi-chunk source must materialise without error");
        assert_eq!(
            materialised, data,
            "a multi-chunk streamed source must be materialised in full, up to EOF",
        );
    }

    /// A genuine I/O error mid-materialise (after >= 1 full `CHUNK`) must NOT be
    /// mistaken for end-of-input. `materialise_streamed` must surface the error
    /// rather than return the silently TRUNCATED prefix accumulated so far - the
    /// resident re-parse of such a prefix drops the tail xref and can
    /// brute-force-repair a partial document (ISO 32000-1 §7.5.4 / §7.5.8). The
    /// source serves one full 1 MiB chunk, then fails every later read; the old
    /// `read_range` path collapsed that failure to an empty chunk and broke the
    /// loop with a 1 MiB truncated prefix.
    #[test]
    fn materialise_streamed_mid_stream_io_error_is_not_truncation() {
        const CHUNK: u64 = 1 << 20; // Mirrors `materialise_streamed`'s chunk size.

        struct FailAfterFirstChunk;

        impl crate::ReadAt for FailAfterFirstChunk {
            fn len(&self) -> u64 {
                // Claim several chunks so the loop must read past the failure point.
                3 * CHUNK
            }

            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, crate::ReadAtError> {
                if offset >= CHUNK {
                    // Transient failure once the first full chunk has been served.
                    return Err(crate::ReadAtError::Io);
                }
                let n = usize::try_from(CHUNK - offset)
                    .unwrap_or(usize::MAX)
                    .min(buf.len());
                buf[..n].fill(0xAB);
                Ok(n)
            }
        }

        let result = super::materialise_streamed(
            &crate::PdfData::streamed(FailAfterFirstChunk),
            crate::data::FULL_FALLBACK_MAX,
        );
        assert_eq!(
            result,
            Err(crate::ReadAtError::Io),
            "a mid-materialise I/O error must fail the load, not yield a truncated prefix",
        );
    }

    /// A source that never signals end-of-input - always returning a full
    /// non-empty read, violating the `ReadAt` contract that `0` bytes means
    /// `offset >= len()` - must NOT drive `materialise_streamed` to accumulate
    /// without bound (an allocation abort / OOM). The running total is hard-capped
    /// independently of the source's EOF signal: once it exceeds `cap` the load
    /// fails (`Err`) rather than growing further, so the peak allocation is
    /// bounded by `cap + CHUNK`. A small `cap` here observes the bound without a
    /// multi-GiB allocation; production passes `FULL_FALLBACK_MAX`, the same
    /// whole-file bound `full_bytes` enforces.
    #[test]
    fn materialise_streamed_non_terminating_source_is_capped() {
        use std::sync::atomic::{AtomicU64, Ordering};

        /// Records the total bytes served through a shared handle (the source is
        /// moved into `PdfData`, so the total is read back after the call).
        struct NeverEofSource(std::sync::Arc<AtomicU64>);

        impl crate::ReadAt for NeverEofSource {
            fn len(&self) -> u64 {
                // Irrelevant to the loop (which ignores the claimed length); a
                // huge value only underlines that the source lies about its size.
                u64::MAX
            }

            fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize, crate::ReadAtError> {
                // Always fill the whole buffer: a non-empty read at every offset,
                // so the loop's EOF check never fires and only the cap can stop it.
                buf.fill(0xAB);
                self.0.fetch_add(buf.len() as u64, Ordering::Relaxed);
                Ok(buf.len())
            }
        }

        const CHUNK: u64 = 1 << 20; // Mirrors `materialise_streamed`'s chunk size.
        const CAP: u64 = 4 << 20; // 4 MiB: several chunks, but a cheap allocation.

        let served = std::sync::Arc::new(AtomicU64::new(0));
        let result = super::materialise_streamed(
            &crate::PdfData::streamed(NeverEofSource(served.clone())),
            CAP,
        );
        assert_eq!(
            result,
            Err(crate::ReadAtError::Io),
            "a source that never signals end-of-input must fail the load at the cap, \
             not accumulate without bound",
        );

        let served = served.load(Ordering::Relaxed);
        assert!(
            served <= CAP + CHUNK,
            "materialisation of a non-terminating source must be bounded by cap + CHUNK \
             ({} bytes); it served {served} bytes",
            CAP + CHUNK,
        );
    }

    /// Finding 2 (bounded reservation): `materialise_streamed` must grow its
    /// buffer with an EXACT reservation, never `Vec`'s amortised doubling.
    /// Doubling would let a `buf` at the cap reserve toward `2 * cap` (a 4 GiB
    /// reservation at the 2 GiB production cap, or a 32-bit capacity-overflow
    /// panic) before the cap check fired. A terminating multi-chunk source
    /// materialises in full; because its length crosses a doubling boundary, a
    /// plain `extend_from_slice` would overshoot the buffer's capacity to the next
    /// power of two, whereas `try_reserve_exact` keeps capacity tracking length.
    #[test]
    fn materialise_streamed_reserves_without_amortised_doubling() {
        const CHUNK: usize = 1 << 20; // Mirrors `materialise_streamed`'s chunk size.
        // 2.5 MiB: crosses the 2->4 MiB doubling boundary, so `extend_from_slice`
        // alone would reserve 4 MiB for a 2.5 MiB buffer.
        const LEN: usize = (2 << 20) + (1 << 19);
        let data = vec![0xCD_u8; LEN];
        let materialised = super::materialise_streamed(
            &crate::PdfData::streamed(SliceReadAt(data)),
            crate::data::FULL_FALLBACK_MAX,
        )
        .expect("a terminating multi-chunk source materialises");
        assert_eq!(materialised.len(), LEN, "the whole source is materialised");
        assert!(
            materialised.capacity() <= materialised.len() + CHUNK,
            "materialise_streamed must reserve exactly (no amortised doubling): a \
             {LEN}-byte buffer reserved {} bytes of capacity",
            materialised.capacity(),
        );
    }
}
