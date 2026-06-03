//! The starting point for reading PDF files.

use crate::PdfData;
use crate::object::{Dict, Object};
use crate::page::Pages;
use crate::page::cached::CachedPages;
use crate::reader::Reader;
use crate::sync::Arc;
use crate::xref::{XRef, XRefError, fallback, root_xref};

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
    /// Convenience accessor for [`XRef::latest_trailer`]. See that
    /// method for semantics — in particular, for a linearised document
    /// this differs from [`Self::trailer`] because `startxref` points
    /// at the first-page xref section while `/Prev` reaches the main
    /// xref at the tail.
    ///
    /// Requires the `inspect` feature.
    #[cfg(feature = "inspect")]
    pub fn latest_trailer(&self) -> Option<Dict<'_>> {
        self.xref.latest_trailer()
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
            include_bytes!("../../hayro-tests/pdfs/custom/andler-optimal-lot-size.pdf");
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
            include_bytes!("../../hayro-tests/pdfs/custom/andler-optimal-lot-size.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("fixture loads");
        let first = pdf.file_layout() as *const _;
        let second = pdf.file_layout() as *const _;
        // Cache returns the same reference, not a fresh computation.
        assert_eq!(first, second);
    }

    #[test]
    #[ignore = "vendored copy omits hayro-tests/downloads/ — restore the upstream hayro-tests \
                submodule to run this fixture; the mangwhap vendor of hayro-syntax \
                ships only the `pdfs/custom/` fixtures referenced by `include_bytes!`"]
    fn pdf_version_header() {
        let data = std::fs::read("../hayro-tests/downloads/pdfjs/alphatrans.pdf").unwrap();
        let pdf = Pdf::new(data).unwrap();

        assert_eq!(pdf.version(), PdfVersion::Pdf17);
    }

    #[test]
    #[ignore = "vendored copy omits hayro-tests/downloads/ — see pdf_version_header"]
    fn pdf_version_catalog() {
        let data = std::fs::read("../hayro-tests/downloads/pdfbox/2163.pdf").unwrap();
        let pdf = Pdf::new(data).unwrap();

        assert_eq!(pdf.version(), PdfVersion::Pdf14);
    }
}
