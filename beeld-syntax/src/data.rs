use crate::object::ObjectIdentifier;
use crate::object::Stream;
use crate::read_at::ReadAt;
use crate::reader::ReaderContext;
use crate::sync::FxHashMap;
use crate::sync::{Arc, Mutex, MutexExt, OnceLock};
use crate::util::SegmentList;
use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::fmt::{Debug, Formatter};

/// A refcounted handle to a resident PDF byte buffer.
#[cfg(feature = "std")]
type ResidentBytes = Arc<dyn AsRef<[u8]> + Send + Sync>;
#[cfg(not(feature = "std"))]
type ResidentBytes = Arc<dyn AsRef<[u8]>>;

/// A refcounted handle to a positioned-read PDF source.
#[cfg(feature = "std")]
type StreamSource = Arc<dyn ReadAt + Send + Sync>;
#[cfg(not(feature = "std"))]
type StreamSource = Arc<dyn ReadAt>;

/// A streaming byte source plus a lazily-materialised whole-file cache.
struct StreamedData {
    source: StreamSource,
    len: u64,
    // Whole-file materialisation, populated only when a whole-file
    // operation (the brute-force xref rebuild, or an `inspect` whole-file
    // scan) calls `PdfData::as_ref`. The streaming hot path never touches
    // it. Shared across `PdfData` clones via the enclosing `Arc`.
    full: OnceLock<Vec<u8>>,
}

impl StreamedData {
    fn full_bytes(&self) -> &[u8] {
        self.full
            .get_or_init(|| {
                let len = usize::try_from(self.len).unwrap_or(usize::MAX);
                self.source.read_range(0, len).unwrap_or_default()
            })
            .as_slice()
    }
}

#[derive(Clone)]
enum PdfDataInner {
    Resident(ResidentBytes),
    Streamed(Arc<StreamedData>),
}

/// A container for the bytes of a PDF file.
///
/// Either a contiguous resident buffer (the default, zero-copy) or a
/// positioned-read [`ReadAt`] source for streaming — see
/// [`PdfData::streamed`] / [`crate::Pdf::new_with_reader`]. The resident
/// path is byte-for-byte the original behaviour; the streamed path reads
/// each accessed object's byte range on demand and only materialises the
/// whole file on a documented whole-file fallback.
#[derive(Clone)]
pub struct PdfData {
    inner: PdfDataInner,
}

impl Debug for PdfData {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "PdfData {{ ... }}")
    }
}

impl PdfData {
    /// Build a streaming `PdfData` from a positioned-read source. The total
    /// length is taken from [`ReadAt::len`] at construction; the file is
    /// never fully buffered unless a whole-file fallback forces it.
    #[cfg(feature = "std")]
    pub fn streamed<S: ReadAt + Send + Sync + 'static>(source: S) -> Self {
        let len = source.len();
        Self {
            inner: PdfDataInner::Streamed(Arc::new(StreamedData {
                source: Arc::new(source),
                len,
                full: OnceLock::new(),
            })),
        }
    }

    /// Build a streaming `PdfData` from a positioned-read source.
    #[cfg(not(feature = "std"))]
    pub fn streamed<S: ReadAt + 'static>(source: S) -> Self {
        let len = source.len();
        Self {
            inner: PdfDataInner::Streamed(Arc::new(StreamedData {
                source: Arc::new(source),
                len,
                full: OnceLock::new(),
            })),
        }
    }

    /// The total length of the PDF in bytes, without materialising a
    /// streaming source.
    pub fn len(&self) -> u64 {
        match &self.inner {
            PdfDataInner::Resident(b) => (**b).as_ref().len() as u64,
            PdfDataInner::Streamed(s) => s.len,
        }
    }

    /// Whether the source is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AsRef<[u8]> for PdfData {
    fn as_ref(&self) -> &[u8] {
        match &self.inner {
            PdfDataInner::Resident(b) => (**b).as_ref(),
            PdfDataInner::Streamed(s) => s.full_bytes(),
        }
    }
}

#[cfg(feature = "std")]
impl<T: AsRef<[u8]> + Send + Sync + 'static> From<Arc<T>> for PdfData {
    fn from(data: Arc<T>) -> Self {
        Self {
            inner: PdfDataInner::Resident(data),
        }
    }
}

#[cfg(not(feature = "std"))]
impl<T: AsRef<[u8]> + 'static> From<Arc<T>> for PdfData {
    fn from(data: Arc<T>) -> Self {
        Self {
            inner: PdfDataInner::Resident(data),
        }
    }
}

impl From<Vec<u8>> for PdfData {
    fn from(data: Vec<u8>) -> Self {
        Self {
            inner: PdfDataInner::Resident(Arc::new(data)),
        }
    }
}

/// A structure for storing the data of the PDF.
// To explain further: This crate uses a zero-parse approach, meaning that objects like
// dictionaries or arrays always store the underlying data and parse objects lazily as needed,
// instead of allocating the data and storing it in an owned way. However, the problem is that
// not all data is readily available in the original data of the PDF: Objects can also be
// stored in an object streams, in which case we first need to decode the stream before we can
// access the data.
//
// The purpose of `Data` is to allow us to access the original data as well as maybe decoded data
// by faking the same lifetime, so that we don't run into lifetime issues when dealing with
// PDF objects that actually stem from different data sources.
pub(crate) struct Data {
    data: PdfData,
    // 32 segments are more than enough as we can't have more objects than this.
    decoded: SegmentList<Option<Vec<u8>>, 32>,
    map: Mutex<FxHashMap<ObjectIdentifier, usize>>,
}

impl Debug for Data {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "Data {{ ... }}")
    }
}

impl Data {
    /// Create a new `Data` structure.
    pub(crate) fn new(data: PdfData) -> Self {
        Self {
            data,
            decoded: SegmentList::new(),
            map: Mutex::new(FxHashMap::default()),
        }
    }

    /// Get access to the original data of the PDF.
    pub(crate) fn get(&self) -> &PdfData {
        &self.data
    }

    /// Get access to the data of a decoded object stream.
    pub(crate) fn get_with(&self, id: ObjectIdentifier, ctx: &ReaderContext<'_>) -> Option<&[u8]> {
        if let Some(&idx) = self.map.get().get(&id) {
            self.decoded.get(idx)?.as_deref()
        } else {
            // Block scope to keep the lock short-lived.
            let idx = {
                let mut locked = self.map.get();
                let idx = locked.len();
                locked.insert(id, idx);
                idx
            };
            self.decoded
                .get_or_init(idx, || {
                    let stream = ctx.xref().get_with::<Stream<'_>>(id, ctx)?;
                    stream.decoded().ok().map(Cow::into_owned)
                })
                .as_deref()
        }
    }
}
