//! The starting point for reading PDF files.

use crate::PdfData;
use crate::ReadAt;
use crate::object::{Dict, Object};
use crate::page::Pages;
use crate::page::cached::CachedPages;
use crate::reader::Reader;
use crate::sync::Arc;
use crate::xref::{XRef, XRefError, fallback, root_xref, root_xref_streamed};

pub use crate::crypto::DecryptionError;
use crate::metadata::Metadata;

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
        // Fall back to a resident parse (materialise) for multi-section /
        // hybrid / malformed xref, so `get_with` uses the resident path and
        // never double-reads.
        if data.is_streamed() {
            if let Some(pdf) = Self::try_open_streamed(&data, password) {
                return Ok(pdf);
            }
            let len = usize::try_from(data.len()).unwrap_or(usize::MAX);
            return Self::new_with_password(PdfData::from(data.read_range(0, len)), password);
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
    /// Returns `None` when the file needs a resident parse (multi-section /
    /// hybrid / malformed xref), so the caller can fall back.
    fn try_open_streamed(data: &PdfData, password: &str) -> Option<Self> {
        let file_len = data.len();
        let head_len = core::cmp::min(2000, usize::try_from(file_len).unwrap_or(usize::MAX));
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

    /// Return the trailer dictionary pinned by `startxref`.
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

    /// Return the document's encryption dictionary, if any.
    ///
    /// Convenience accessor for [`XRef::encryption_dict`]. See that method
    /// for behaviour and the re-parse performance note.
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

fn find_version(data: &[u8]) -> Option<PdfVersion> {
    let data = &data[..data.len().min(2000)];
    let mut r = Reader::new(data);

    while r.forward_tag(b"%PDF-").is_none() {
        r.read_byte()?;
    }

    PdfVersion::from_bytes(r.tail()?)
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
}
