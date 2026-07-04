use crate::object::ObjectIdentifier;
use crate::object::Stream;
use crate::read_at::{ReadAt, ReadAtError};
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

/// Upper bound on the streamed whole-file materialisation.
///
/// The streaming hot path never needs the whole file; this buffer is forced
/// only by a whole-file operation - the brute-force xref rebuild after a
/// malformed cross-reference table (ISO 32000-1 §7.5.4 / §7.5.8) fails to
/// parse, or an `inspect` whole-file scan. The bound stops a corrupt or
/// adversarial [`ReadAt::len`] from forcing an unbounded allocation (and, on a
/// 32-bit target, a `usize`-overflowing one). A file larger than this simply
/// cannot use the whole-file fallback; the streaming path still serves every
/// well-formed object without it. A brute-force rebuild triggered on such a
/// file is skipped rather than run against the refused, empty materialisation
/// (which would wipe the good streamed table over a single malformed object) -
/// see `PdfData::whole_file_fallback_available` and `XRef::repair`.
pub(crate) const FULL_FALLBACK_MAX: u64 = 2 << 30; // 2 GiB.

/// Upper bound on a single streamed object's on-demand window.
///
/// Deliberately distinct from - and larger than - the whole-file
/// materialisation cap [`FULL_FALLBACK_MAX`]. The highest-offset live object's
/// window ends at the file-length sentinel - the *untrusted* [`ReadAt::len`]
/// (see `XRef::sorted_offsets`) - so its size cannot be taken on trust and some
/// bound must cap it. Reusing the 2 GiB *whole-file* cap there was a defect:
/// streaming is precisely the `> 2 GiB` use case (see [`PdfData::streamed`]), so
/// a single legitimate last object - a large embedded file, image, or font - can
/// itself exceed the whole-file cap and must still be read in full, not
/// truncated to it and then, failing to parse, silently resolved to the null
/// object (ISO 32000-1 §7.3.10). This larger cap lets such an object through
/// while still stopping a corrupt or adversarial source that over-reports its
/// length and, violating the [`ReadAt`] end-of-input contract, never signals EOF
/// from growing a single window without bound (a memory-amplification `DoS`).
///
/// POLICY (tunable): the value trades the largest single object that can be
/// streamed against the largest window an adversarial highest-offset source can
/// force to materialise. A well-behaved (terminating) source is always bounded
/// by its real content, so this ceiling bites only an object genuinely larger
/// than it, or an end-of-input-contract-violating source. Only the highest-offset
/// object is governed by this cap; every other object is bounded by the next
/// live object's real offset (always `< len`) and read in full at any size.
pub(crate) const OBJECT_WINDOW_MAX: u64 = 8 << 30; // 8 GiB.

impl StreamedData {
    /// Materialise the whole file for the whole-file fallback, reading through
    /// `data` - the enclosing [`PdfData`] that wraps this `StreamedData` - in
    /// bounded chunks via `read_window`. The enclosing handle is passed in
    /// rather than reconstructed because the caching `OnceLock` lives on `self`
    /// while the bounded-chunk reader is keyed on the `PdfData`.
    fn full_bytes(&self, data: &PdfData) -> &[u8] {
        // Serve the materialised buffer if a previous call cached one.
        if let Some(bytes) = self.full.get() {
            return bytes.as_slice();
        }
        // Bound the fallback and never cache content fabricated on failure: a
        // length that does not fit `usize` or exceeds the cap is refused
        // (rather than allocating `usize::MAX`), and a genuine source failure
        // yields an *uncached* empty slice so a later whole-file access can
        // retry - mirroring `Data::object_window` (S3). Only a successful read
        // (including a legitimately empty source) populates the `OnceLock`, so a
        // transient failure no longer poisons every later whole-file consumer
        // (repair rebuild, inspect scan) for the document's lifetime.
        if usize::try_from(self.len).is_err() {
            error!(
                "streamed whole-file fallback refused: length {} exceeds usize",
                self.len
            );
            return &[];
        }
        if self.len > FULL_FALLBACK_MAX {
            error!(
                "streamed whole-file fallback refused: length {} exceeds {}-byte cap",
                self.len, FULL_FALLBACK_MAX
            );
            return &[];
        }
        // Read the whole-file window in bounded 1 MiB chunks via `read_window`
        // rather than one `read_range(0, len)`: `ReadAt::read_range`'s default
        // allocates `vec![0; len]` up front, sizing an eager (and zeroing)
        // allocation from the *untrusted* `ReadAt::len` (up to the 2 GiB cap) -
        // so an over-reported length forces a giant transient allocation
        // regardless of the bytes served, and a source that cannot satisfy it
        // aborts the host process (`handle_alloc_error`). Chunked, the eager
        // allocation is one CHUNK and the buffer grows with `try_reserve_exact`
        // (`Err`, not abort) tracking the bytes actually served; an allocation
        // failure surfaces as `Err` and takes the uncached empty-slice path
        // below, degrading a too-large streamed file to a graceful null read
        // (ISO 32000-1 §7.3.10) instead of aborting - matching the sibling
        // materialisers `read_window` and `materialise_streamed`.
        match read_window(data, 0, self.len, WindowGrowth::Exact) {
            Ok(bytes) => self.full.get_or_init(|| bytes).as_slice(),
            Err(e) => {
                error!("streamed whole-file fallback read failed: {}", e);
                &[]
            }
        }
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

    /// Whether this is a streaming (positioned-read) source.
    pub(crate) fn is_streamed(&self) -> bool {
        matches!(self.inner, PdfDataInner::Streamed(_))
    }

    /// Whether the whole-file fallback (`AsRef::as_ref` -> `full_bytes`) can
    /// materialise this source's bytes. A resident source always can. A
    /// streamed source can only when its length both fits `usize` and is within
    /// `FULL_FALLBACK_MAX`; past either guard `full_bytes` refuses and yields an
    /// empty slice rather than allocating. A whole-file consumer - notably the
    /// brute-force xref rebuild (`XRef::repair`) - must consult this before
    /// treating an empty `as_ref` as authoritative, otherwise it would mistake
    /// the refused materialisation for a document with no objects and discard
    /// the good streamed table.
    pub(crate) fn whole_file_fallback_available(&self) -> bool {
        match &self.inner {
            PdfDataInner::Resident(_) => true,
            // Mirror the two deterministic refusals in `StreamedData::full_bytes`:
            // a length that does not fit `usize`, or one past the cap, yields an
            // empty `as_ref` rather than a materialised buffer.
            PdfDataInner::Streamed(s) => {
                usize::try_from(s.len).is_ok() && s.len <= FULL_FALLBACK_MAX
            }
        }
    }

    /// Read the byte range `[offset, offset + len)` into an owned buffer,
    /// truncated at end-of-file, distinguishing a genuine source failure
    /// (`Err`) from a short read at end-of-file (`Ok` of fewer bytes). Unlike
    /// [`PdfData::read_range`] this never substitutes empty bytes for an I/O
    /// error, so a streamed object read cannot cache fabricated empty content.
    /// For a resident source this copies the slice and never fails.
    pub(crate) fn try_read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>, ReadAtError> {
        match &self.inner {
            PdfDataInner::Resident(b) => {
                let all = (**b).as_ref();
                let start = usize::try_from(offset).unwrap_or(usize::MAX).min(all.len());
                let end = start.saturating_add(len).min(all.len());
                Ok(all[start..end].to_vec())
            }
            PdfDataInner::Streamed(s) => s.source.read_range(offset, len),
        }
    }

    /// Read the byte range `[offset, offset + len)` into an owned buffer,
    /// truncated at end-of-file. A genuine source failure yields an empty
    /// buffer: callers on the open / whole-file-fallback path treat that as
    /// "unreadable -> fall back". The object hot path uses
    /// [`PdfData::try_read_range`] instead, so a positioned-read failure is
    /// never mistaken there for real empty content.
    pub(crate) fn read_range(&self, offset: u64, len: usize) -> Vec<u8> {
        self.try_read_range(offset, len).unwrap_or_default()
    }
}

impl AsRef<[u8]> for PdfData {
    fn as_ref(&self) -> &[u8] {
        match &self.inner {
            PdfDataInner::Resident(b) => (**b).as_ref(),
            PdfDataInner::Streamed(s) => s.full_bytes(self),
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
    // Streaming object-window cache: each accessed object's byte range is
    // pread from the source once and served as `&[u8]` with the same
    // lifetime as the decoded-object-stream arena above (stable addresses,
    // `&self`-insert). Empty/unused for a resident source.
    windows: SegmentList<Vec<u8>, 32>,
    window_map: Mutex<FxHashMap<u64, usize>>,
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
            windows: SegmentList::new(),
            window_map: Mutex::new(FxHashMap::default()),
        }
    }

    /// Get access to the original data of the PDF.
    pub(crate) fn get(&self) -> &PdfData {
        &self.data
    }

    /// Get access to the data of a decoded object stream.
    pub(crate) fn get_with(&self, id: ObjectIdentifier, ctx: &ReaderContext<'_>) -> Option<&[u8]> {
        // Resolve the arena slot atomically: get-or-insert under a single lock
        // acquisition, so two threads racing to first-touch DIFFERENT ids
        // cannot both claim the same slot index (which would alias one object's
        // decoded bytes onto the other). A losing racer for the SAME id observes
        // the winner's index. The lock is released before `get_or_init`, so the
        // re-entrant stream resolution below never deadlocks.
        let idx = {
            let mut locked = self.map.get();
            let next = locked.len();
            *locked.entry(id).or_insert(next)
        };
        self.decoded
            .get_or_init(idx, || {
                let stream = ctx.xref().get_with::<Stream<'_>>(id, ctx)?;
                stream.decoded().ok().map(Cow::into_owned)
            })
            .as_deref()
    }

    /// Read the byte window `[offset, end_bound)` for a streamed object,
    /// caching it in the window arena so the returned `&[u8]` lives as long
    /// as `&self` (stable addresses, like the decoded-object-stream arena).
    /// Re-entrancy-safe: the map lock is released before the positioned read
    /// (which goes through `&self`), so a nested object resolution - e.g. an
    /// indirect `/Length` - does not deadlock.
    pub(crate) fn object_window(&self, offset: u64, end_bound: u64) -> Result<&[u8], ReadAtError> {
        // Resolve the window slot atomically: get-or-insert the slot index under
        // a single lock acquisition. Two threads racing to first-touch DIFFERENT
        // offsets get DISTINCT indices, so one object's bytes are never aliased
        // into another's slot; a racer for the SAME offset observes the winner's
        // index. The lock is released before the positioned read below, so a
        // nested resolution (e.g. an indirect `/Length`) cannot deadlock.
        let idx = {
            let mut locked = self.window_map.get();
            let next = locked.len();
            *locked.entry(offset).or_insert(next)
        };
        // Already materialised: serve the cached window.
        if let Some(window) = self.windows.get(idx) {
            return Ok(window.as_slice());
        }
        // Size the window, then read it incrementally. `end_bound` for the
        // highest-offset live object is the file-length sentinel - the
        // *untrusted* `ReadAt::len` (see `XRef::sorted_offsets`) - so `window_len`
        // clamps only that case to the dedicated object-window cap
        // `OBJECT_WINDOW_MAX`, NOT the smaller whole-file cap; a real next-object
        // span is trusted and used in full, so a legitimate object larger than
        // the whole-file cap (streaming is precisely the `> 2 GiB` use case) is
        // not truncated and silently resolved to the null object. `read_window`
        // reads in bounded chunks, so trusting a large span never forces a giant
        // eager `vec![0; len]` and an over-reported length still cannot amplify: the
        // allocation tracks the bytes actually served. A genuine source failure
        // is surfaced as `Err` (distinct from a window that reads but parses
        // wrongly) and NOT cached as an empty window, so the caller resolves just
        // this object to the null object (ISO 32000-1 §7.3.10) and retries later,
        // rather than escalating one transient read to a whole-file repair. Only
        // a successful read populates the slot.
        let (want, growth) = window_len(offset, end_bound, self.data.len());
        let bytes = read_window(&self.data, offset, want, growth)?;
        Ok(self.windows.get_or_init(idx, || bytes).as_slice())
    }
}

/// How [`read_window`] grows its accumulation buffer as chunks arrive.
///
/// Both policies read in bounded chunks and stop at end-of-input, so the buffer
/// only grows to the bytes actually served - neither reserves the requested span
/// up front, so an over-reporting source can never amplify a short read into a
/// giant allocation. They differ only in the reservation shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WindowGrowth {
    /// Amortised doubling, capped at the requested span. Used when the span is a
    /// trusted next-object offset (bounded by real parsed content,
    /// `end_bound < file_len`): a legitimately large object - the `> 2 GiB`
    /// streaming use case - is accumulated in O(log) reallocations, i.e. O(span)
    /// copy traffic, and the cap keeps the buffer from over-allocating past the
    /// object's real size.
    Amortised,
    /// Exact per-chunk reservation. Used when the span is an untrusted ceiling -
    /// the whole-file fallback over the reported [`ReadAt::len`], or the
    /// highest-offset object's window capped at [`OBJECT_WINDOW_MAX`]. The buffer
    /// stays minimal (no over-allocation ahead of the served bytes, keeping the
    /// long-lived whole-file cache tight); the O(span^2 / CHUNK) copy traffic
    /// this costs bites only these cold / adversarial paths, never a legitimate
    /// object read.
    Exact,
}

/// The number of bytes to read for a streamed object window at `offset` whose
/// exclusive end is `end_bound`, given the source's reported `file_len`, and the
/// [`WindowGrowth`] policy [`read_window`] should use to accumulate it.
///
/// `end_bound` for the highest-offset live object is the file-length sentinel -
/// the *untrusted* [`ReadAt::len`] (see `XRef::sorted_offsets`) - so clamp only
/// that case (and a corrupt offset sorted past it) to [`OBJECT_WINDOW_MAX`],
/// stopping a source that over-reports its length and never signals end-of-input
/// from sizing an unbounded read. The bound is the dedicated *object-window* cap,
/// NOT the smaller *whole-file* cap [`FULL_FALLBACK_MAX`]: streaming is the
/// `> 2 GiB` use case, so a single legitimate last object larger than the
/// whole-file cap is read in full up to [`OBJECT_WINDOW_MAX`] rather than
/// truncated to 2 GiB and, failing to parse, silently resolved to the null
/// object (ISO 32000-1 §7.3.10). For every other object `end_bound` is the next
/// object's real offset, always `< file_len`, so the span is trusted and used in
/// full at any size. Trusting a large span is safe because `read_window` reads
/// incrementally, so it never turns into a giant eager allocation.
///
/// The returned policy is [`WindowGrowth::Amortised`] for a trusted span (the
/// span is bounded by real content, so amortised doubling capped at the span is
/// safe and gives O(span) copy) and [`WindowGrowth::Exact`] for the untrusted
/// sentinel ceiling (grown exactly, staying minimal on that cold / adversarial
/// path). The trusted branch is *uncapped* - the span can be as large as the
/// (untrusted) `file_len` if a corrupt xref places a distant next offset - so the
/// span must never be reserved up front; amortised growth stays served-driven.
fn window_len(offset: u64, end_bound: u64, file_len: u64) -> (u64, WindowGrowth) {
    let span = end_bound.saturating_sub(offset);
    if end_bound >= file_len {
        // The untrusted file-length sentinel (or a corrupt offset past it):
        // bound by the dedicated object-window cap, not the whole-file cap, and
        // grow exactly so the buffer never over-allocates ahead of the bytes an
        // over-reporting source actually serves.
        (span.min(OBJECT_WINDOW_MAX), WindowGrowth::Exact)
    } else {
        // A trusted next-object offset from the parsed xref, always < file_len:
        // the span is bounded by real content, so grow with amortised doubling
        // capped at the span - O(span) copy for a legitimately large object, not
        // the O(span^2 / CHUNK) of exact per-chunk growth.
        (span, WindowGrowth::Amortised)
    }
}

/// Read `[offset, offset + max_len)` from `data` incrementally, stopping at
/// end-of-input, returning the bytes actually available.
///
/// A single `try_read_range(offset, max_len)` would allocate `vec![0; max_len]`
/// up front ([`ReadAt::read_range`]'s default), so a large trusted span - or an
/// over-reported sentinel - could force a giant eager allocation from a source
/// that then delivers only a few bytes (a memory-amplification `DoS`). Reading in
/// bounded chunks caps the eager allocation at one `CHUNK`; the buffer grows only
/// as bytes are served (never the requested span up front) and yields `Err`
/// rather than aborting on allocation failure (a 32-bit-safe bound); and an
/// honest end-of-input stops the read early, so the allocation tracks the bytes
/// actually served rather than the requested span. A genuine source failure
/// propagates as `Err`, never a truncated buffer masquerading as the object's
/// bytes.
///
/// `growth` selects the reservation shape (see [`WindowGrowth`]): a trusted span
/// grows with amortised doubling *capped at the span* - O(span) copy for a
/// legitimately large object, and no over-allocation past its real size - while
/// an untrusted ceiling grows exactly, staying minimal at the cost of
/// O(span^2 / CHUNK) copy on those cold / adversarial paths. Neither reserves the
/// span up front, so the amplification bound holds under either policy.
fn read_window(
    data: &PdfData,
    offset: u64,
    max_len: u64,
    growth: WindowGrowth,
) -> Result<Vec<u8>, ReadAtError> {
    const CHUNK: u64 = 1 << 20; // 1 MiB.
    let mut buf = Vec::new();
    let mut pos = offset;
    let end = offset.saturating_add(max_len);
    // The window never accumulates more than `max_len` bytes (the loop stops at
    // `end`), so the amortised policy caps its doubling here without ever
    // under-reserving for a served chunk.
    let cap = usize::try_from(max_len).unwrap_or(usize::MAX);
    while pos < end {
        let chunk_len = usize::try_from((end - pos).min(CHUNK)).unwrap_or(usize::MAX);
        let chunk = data.try_read_range(pos, chunk_len)?;
        if chunk.is_empty() {
            break;
        }
        pos = pos.saturating_add(chunk.len() as u64);
        match growth {
            // Amortised doubling capped at the trusted span: O(span) copy for a
            // legitimately large object, yet the buffer never over-allocates past
            // the object's real size. `clamp`'s lower bound `needed` keeps a
            // source that over-returns a chunk reserving enough; its upper bound
            // `cap.max(needed)` is the span (or `needed` in that degenerate
            // over-return case), so the doubling never overshoots it.
            WindowGrowth::Amortised => {
                let needed = buf.len().saturating_add(chunk.len());
                if needed > buf.capacity() {
                    let target = buf
                        .capacity()
                        .saturating_mul(2)
                        .clamp(needed, cap.max(needed));
                    buf.try_reserve_exact(target - buf.len())
                        .map_err(|_| ReadAtError::Io)?;
                }
            }
            // Exact per-chunk reservation: the buffer stays minimal, never
            // over-allocating ahead of the served bytes on the untrusted-ceiling
            // paths (whole-file fallback / over-reported sentinel).
            WindowGrowth::Exact => {
                buf.try_reserve_exact(chunk.len())
                    .map_err(|_| ReadAtError::Io)?;
            }
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_at::{ReadAt, ReadAtError};
    use crate::xref::root_xref;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc as StdArc, Barrier};

    /// A positioned-read source over an in-memory buffer.
    struct SliceSource(Vec<u8>);

    impl ReadAt for SliceSource {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadAtError> {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(self.0.len());
            let n = (self.0.len() - start).min(buf.len());
            buf[..n].copy_from_slice(&self.0[start..start + n]);
            Ok(n)
        }
    }

    /// S1 (aliasing): concurrent first-touch of DISTINCT object windows must
    /// never alias one window's bytes onto another's arena slot. Many threads
    /// hammer the SAME offsets from a FRESH cache: the same-offset first-touch
    /// races corrupt the slot-index bookkeeping unless the check-and-insert is
    /// atomic, after which a later DISTINCT offset reuses an occupied slot and
    /// reads the wrong object's bytes. Each window here encodes its own offset,
    /// so any aliasing surfaces as a mismatched (or absent) read.
    #[test]
    fn object_window_concurrent_first_touch_no_aliasing() {
        const N: u64 = 48;
        const STRIDE: u64 = 8;
        let mut buf = vec![0_u8; (N * STRIDE) as usize];
        for i in 0..N {
            let off = i * STRIDE;
            buf[off as usize..(off + STRIDE) as usize].copy_from_slice(&off.to_le_bytes());
        }

        for _ in 0..200 {
            let data = StdArc::new(Data::new(PdfData::streamed(SliceSource(buf.clone()))));
            let barrier = StdArc::new(Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let data = data.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        for i in 0..N {
                            let off = i * STRIDE;
                            let w = data
                                .object_window(off, off + STRIDE)
                                .expect("window present");
                            assert_eq!(
                                w,
                                off.to_le_bytes().as_slice(),
                                "window at offset {off} must encode its own offset, not an aliased one",
                            );
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join()
                    .expect("worker thread panicked - window aliasing observed");
            }
        }
    }

    /// Build a resident PDF whose objects `4..4 + n` are distinct streams, each
    /// decoding to a payload that encodes its own object number. Objects 1-3 are
    /// a minimal catalog / pages / page the streams are never referenced from, so
    /// each stream is materialised only by an explicit `Data::get_with` lookup.
    /// Returns the bytes and the `(id, decoded-payload)` pairs to assert on.
    fn stream_pdf(n: i32) -> (Vec<u8>, Vec<(i32, Vec<u8>)>) {
        let mut body = Vec::new();
        body.extend_from_slice(b"%PDF-1.7\n");

        let mut offsets: Vec<usize> = Vec::new();
        let fixed: [&[u8]; 3] = [
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>\nendobj\n",
        ];
        for obj in fixed {
            offsets.push(body.len());
            body.extend_from_slice(obj);
        }

        let mut expected: Vec<(i32, Vec<u8>)> = Vec::new();
        for id in 4..4 + n {
            // Distinct per id so an aliased slot surfaces as a byte mismatch.
            let payload = format!("objstm-payload-{id:06}").into_bytes();
            offsets.push(body.len());
            body.extend_from_slice(
                format!("{id} 0 obj\n<< /Length {} >>\nstream\n", payload.len()).as_bytes(),
            );
            body.extend_from_slice(&payload);
            body.extend_from_slice(b"\nendstream\nendobj\n");
            expected.push((id, payload));
        }

        let xoff = body.len();
        let size = 4 + n; // Objects 0..=3 + n.
        let mut xref = format!("xref\n0 {size}\n0000000000 65535 f \n");
        for off in &offsets {
            xref.push_str(&format!("{off:010} 00000 n \n"));
        }
        xref.push_str(&format!(
            "trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xoff}\n%%EOF\n"
        ));
        body.extend_from_slice(xref.as_bytes());

        (body, expected)
    }

    /// S1 (aliasing), decoded-object-stream cache: the sibling of
    /// `object_window_concurrent_first_touch_no_aliasing`, for `Data::get_with`.
    /// Both apply the same atomic get-or-insert slot bookkeeping; only
    /// `object_window` was pinned. Many threads concurrently first-touch DISTINCT
    /// object-stream ids from a FRESH cache: a regression to non-atomic
    /// check-then-insert lets two racers compute the same `next` slot index and
    /// alias one stream's decoded bytes onto another's. Each id here decodes to
    /// its own payload, so any aliasing surfaces as a mismatched read.
    /// `Data::get_with` caches `stream.decoded()` for whatever stream an id
    /// resolves to (its `/Type` is irrelevant to the cache), so distinct plain
    /// streams exercise the slot race exactly as compressed object streams would.
    #[test]
    fn get_with_concurrent_first_touch_no_aliasing() {
        const N: i32 = 24;
        let (bytes, expected) = stream_pdf(N);
        // The xref is read-only and never mutated by a normal-entry stream
        // resolution, so it is shared; only the `Data` arena under test is fresh
        // per iteration, re-running the first-touch race from an empty cache.
        let xref =
            StdArc::new(root_xref(PdfData::from(bytes.clone()), b"").expect("fixture xref parses"));

        for _ in 0..200 {
            let data = StdArc::new(Data::new(PdfData::from(bytes.clone())));
            let barrier = StdArc::new(Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let data = data.clone();
                    let xref = xref.clone();
                    let barrier = barrier.clone();
                    let expected = expected.clone();
                    std::thread::spawn(move || {
                        let ctx = ReaderContext::new(&xref, false);
                        barrier.wait();
                        for (id, want) in &expected {
                            let got = data
                                .get_with(ObjectIdentifier::new(*id, 0), &ctx)
                                .expect("decoded object-stream bytes present");
                            assert_eq!(
                                got,
                                want.as_slice(),
                                "object {id} must decode to its own bytes, not an aliased one",
                            );
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join()
                    .expect("worker thread panicked - decoded-cache aliasing observed");
            }
        }
    }

    /// S3 (I/O errors): a genuine source failure must NOT be cached as an empty
    /// window (which would masquerade as a valid empty object for the
    /// document's lifetime). The source fails the first read at the target
    /// offset, then succeeds; a correct `object_window` returns `None` for the
    /// failed read (caching nothing) and the real bytes on retry. Fabricating
    /// empty bytes would instead hand back `Some(&[])` and pin it forever.
    #[test]
    fn object_window_io_error_not_cached_as_empty() {
        struct FailOnceSource {
            data: Vec<u8>,
            fail_at: u64,
            armed: AtomicBool,
        }

        impl ReadAt for FailOnceSource {
            fn len(&self) -> u64 {
                self.data.len() as u64
            }

            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadAtError> {
                if offset == self.fail_at && self.armed.swap(false, Ordering::SeqCst) {
                    return Err(ReadAtError::Io);
                }
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(self.data.len());
                let n = (self.data.len() - start).min(buf.len());
                buf[..n].copy_from_slice(&self.data[start..start + n]);
                Ok(n)
            }
        }

        let data = Data::new(PdfData::streamed(FailOnceSource {
            data: b"HELLOWORLD".to_vec(),
            fail_at: 2,
            armed: AtomicBool::new(true),
        }));

        assert_eq!(
            data.object_window(2, 6),
            Err(ReadAtError::Io),
            "an I/O error must be surfaced (not fabricated or cached as an empty window)",
        );
        assert_eq!(
            data.object_window(2, 6),
            Ok(&b"LLOW"[..]),
            "the failed read must not have been cached; a retry must succeed",
        );
    }

    /// S4 (fallback guard): a streamed source reporting an enormous length must
    /// not force an unbounded (or, on a 32-bit target, `usize`-overflowing)
    /// whole-file allocation. `as_ref` refuses past the bound and yields an
    /// empty slice rather than panicking / aborting on a giant allocation.
    #[test]
    fn whole_file_fallback_refuses_oversize_length() {
        struct HugeLenSource;

        impl ReadAt for HugeLenSource {
            fn len(&self) -> u64 {
                u64::MAX
            }

            fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, ReadAtError> {
                Ok(0)
            }
        }

        let data = PdfData::streamed(HugeLenSource);
        let bytes: &[u8] = data.as_ref();
        assert!(
            bytes.is_empty(),
            "an oversize whole-file fallback must be refused, not allocated",
        );
    }

    /// The predicate that guards `XRef::repair` must track the source kind and
    /// the materialisation cap: a resident source is always materialisable; a
    /// streamed source only within `FULL_FALLBACK_MAX`. A streamed source past
    /// the cap reports `false`, so repair skips the impossible rebuild rather
    /// than wipe the good streamed table with the refused, empty `as_ref`.
    #[test]
    fn whole_file_fallback_available_tracks_source_and_cap() {
        struct OversizeLen;
        impl ReadAt for OversizeLen {
            fn len(&self) -> u64 {
                FULL_FALLBACK_MAX + 1
            }
            fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, ReadAtError> {
                Ok(0)
            }
        }

        assert!(
            PdfData::from(b"resident".to_vec()).whole_file_fallback_available(),
            "a resident source is always materialisable",
        );
        assert!(
            PdfData::streamed(SliceSource(b"small".to_vec())).whole_file_fallback_available(),
            "a streamed source within the cap is materialisable",
        );
        assert!(
            !PdfData::streamed(OversizeLen).whole_file_fallback_available(),
            "a streamed source past the cap must report the fallback unavailable",
        );
    }

    /// A transient whole-file read failure must NOT be cached as an empty
    /// buffer. `full_bytes` (backing `AsRef`) returns an empty slice WITHOUT
    /// populating its `OnceLock` on error, so a later whole-file access retries
    /// and succeeds - mirroring `object_window`'s S3 behaviour. Caching the
    /// empty buffer would poison every whole-file consumer (repair rebuild,
    /// inspect scan) for the document's lifetime.
    #[test]
    fn whole_file_fallback_io_error_not_cached_as_empty() {
        struct FailOnceWholeFile {
            data: Vec<u8>,
            armed: AtomicBool,
        }

        impl ReadAt for FailOnceWholeFile {
            fn len(&self) -> u64 {
                self.data.len() as u64
            }

            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadAtError> {
                // Fail the first read that starts the whole-file materialisation.
                if offset == 0 && self.armed.swap(false, Ordering::SeqCst) {
                    return Err(ReadAtError::Io);
                }
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(self.data.len());
                let n = (self.data.len() - start).min(buf.len());
                buf[..n].copy_from_slice(&self.data[start..start + n]);
                Ok(n)
            }
        }

        let data = PdfData::streamed(FailOnceWholeFile {
            data: b"HELLOWORLD".to_vec(),
            armed: AtomicBool::new(true),
        });

        // First whole-file access hits the transient failure: an empty slice
        // that must NOT be cached.
        let first: &[u8] = data.as_ref();
        assert!(
            first.is_empty(),
            "a transient whole-file read failure must yield an empty slice",
        );
        // The failure was not cached, so a retry reads the real bytes.
        let second: &[u8] = data.as_ref();
        assert_eq!(
            second, b"HELLOWORLD",
            "the failed whole-file read must not have been cached; a retry must succeed",
        );
    }

    /// Whole-file fallback (`full_bytes`, backing `AsRef`), bounded chunks: the
    /// within-cap materialisation must read in bounded chunks, never one eager
    /// `read_range(0, len)` sized from the untrusted `ReadAt::len`. A source
    /// reporting `len == FULL_FALLBACK_MAX` (within the cap, so `full_bytes`
    /// does NOT refuse it) but serving only a few real bytes must drive only
    /// CHUNK-sized reads: `ReadAt::read_range`'s default allocates `vec![0; len]`,
    /// so an un-chunked call would eagerly allocate (and, on a source that
    /// cannot, abort on) 2 GiB regardless of the bytes served. The recording
    /// source reports the cap as its length and records each requested read
    /// length WITHOUT allocating it, so the test observes the per-read bound
    /// rather than aborting - and still gets the real served bytes back. The
    /// sibling `object_window_over_reported_length_reads_in_bounded_chunks` pins
    /// the same property for the per-object window path.
    #[test]
    fn whole_file_fallback_within_cap_reads_in_bounded_chunks() {
        const CHUNK: u64 = 1 << 20; // Mirrors `read_window`'s chunk size.

        struct OverReportingWithinCap {
            data: Vec<u8>,
            max_len: StdArc<std::sync::atomic::AtomicU64>,
        }

        impl ReadAt for OverReportingWithinCap {
            fn len(&self) -> u64 {
                // Within the whole-file cap, so `full_bytes` takes the read path
                // rather than the oversize refusal - exactly the path that used
                // to size a single `vec![0; len]` from this untrusted length.
                FULL_FALLBACK_MAX
            }

            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadAtError> {
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(self.data.len());
                let n = (self.data.len() - start).min(buf.len());
                buf[..n].copy_from_slice(&self.data[start..start + n]);
                Ok(n)
            }

            fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>, ReadAtError> {
                // Record the requested length WITHOUT allocating it (the default
                // impl would `vec![0; len]` and, at a 2 GiB request, abort), then
                // serve only the bytes actually available.
                self.max_len.fetch_max(len as u64, Ordering::Relaxed);
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(self.data.len());
                let end = start.saturating_add(len).min(self.data.len());
                Ok(self.data[start..end].to_vec())
            }
        }

        let max_len = StdArc::new(std::sync::atomic::AtomicU64::new(0));
        let payload = b"%PDF-1.7\nwhole-file-fallback-payload\n".to_vec();
        let data = PdfData::streamed(OverReportingWithinCap {
            data: payload.clone(),
            max_len: max_len.clone(),
        });

        let bytes: &[u8] = data.as_ref();
        assert_eq!(
            bytes, payload,
            "the whole-file fallback must return the bytes actually served",
        );

        let observed = max_len.load(Ordering::Relaxed);
        assert!(
            observed <= CHUNK,
            "the whole-file fallback must read in bounded chunks, never one giant \
             vec![0; len] sized from an over-reported ReadAt::len; a single read \
             requested {observed} bytes",
        );
    }

    /// Finding 1 (window sizing): the highest-offset object's window - bounded by
    /// the untrusted file-length sentinel (the reported `ReadAt::len`, see
    /// `XRef::sorted_offsets`) - must be clamped by the DEDICATED object-window
    /// cap, NOT the smaller whole-file cap. Clamping it to `FULL_FALLBACK_MAX`
    /// (2 GiB) silently truncated, and so dropped, a legitimate last object
    /// larger than 2 GiB - exactly the `> 2 GiB` streaming use case. A trusted
    /// next-object span is never clamped. Pure arithmetic, so the out-of-cap
    /// spans are exercised without allocating them.
    #[test]
    fn window_len_sentinel_bounded_by_object_window_cap_not_whole_file_cap() {
        // A trusted next-object span (`end_bound < file_len`) larger than either
        // cap is used in full - the object is not truncated - and, being bounded
        // by real content, grows with the amortised (capped-doubling) policy.
        assert_eq!(
            window_len(0, 3 << 30, 5 << 30),
            (3 << 30, WindowGrowth::Amortised),
        );
        // Finding 1: an honest highest-offset object (`end_bound == file_len`)
        // between the whole-file cap and the object-window cap must read in FULL,
        // NOT be truncated to the 2 GiB whole-file cap and then, failing to
        // parse, silently resolved to the null object (ISO 32000-1 §7.3.10). The
        // sentinel span is untrusted, so it grows with the exact policy.
        const {
            assert!(
                (5 << 30) > FULL_FALLBACK_MAX && (5 << 30) < OBJECT_WINDOW_MAX,
                "precondition: 5 GiB is between the whole-file cap and the object-window cap",
            );
        }
        assert_eq!(
            window_len(0, 5 << 30, 5 << 30),
            (5 << 30, WindowGrowth::Exact),
        );
        // At the object-window cap the sentinel still reads in full.
        assert_eq!(
            window_len(0, OBJECT_WINDOW_MAX, OBJECT_WINDOW_MAX),
            (OBJECT_WINDOW_MAX, WindowGrowth::Exact),
        );
        // Past the object-window cap the sentinel IS clamped - the documented
        // ceiling that keeps an over-reporting / non-terminating source bounded.
        assert_eq!(
            window_len(0, OBJECT_WINDOW_MAX + 4096, OBJECT_WINDOW_MAX + 4096),
            (OBJECT_WINDOW_MAX, WindowGrowth::Exact),
        );
        // An over-reported sentinel is clamped to the object-window cap, not
        // sized from the untrusted claim (the retained DoS bound).
        assert_eq!(
            window_len(0, u64::MAX, u64::MAX),
            (OBJECT_WINDOW_MAX, WindowGrowth::Exact),
        );
        // A corrupt offset sorted past the file length (`end_bound > file_len`)
        // yields a zero-length, clamped window rather than a huge read.
        assert_eq!(window_len(100, 100, 50), (0, WindowGrowth::Exact));
        // A small trusted span is returned unchanged, with amortised growth.
        assert_eq!(window_len(10, 30, 1000), (20, WindowGrowth::Amortised));
    }

    /// Finding 1 (no giant eager window read): the highest-offset object's window
    /// ends at the file-length sentinel - the *untrusted* `ReadAt::len`. A source
    /// over-reporting its length must NOT drive a single `vec![0; want]` sized
    /// from the claim: `object_window` reads the window in bounded chunks and
    /// stops at end-of-input, so the source sees only `CHUNK`-sized reads and
    /// serves only the bytes it has. The recording source reports `u64::MAX` and
    /// records each requested read length WITHOUT allocating it, so the test
    /// observes the per-read bound rather than aborting on a giant allocation.
    #[test]
    fn object_window_over_reported_length_reads_in_bounded_chunks() {
        const CHUNK: u64 = 1 << 20; // Mirrors `read_window`'s chunk size.

        struct OverReportingLen {
            data: Vec<u8>,
            max_len: StdArc<std::sync::atomic::AtomicU64>,
        }

        impl ReadAt for OverReportingLen {
            fn len(&self) -> u64 {
                u64::MAX
            }

            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadAtError> {
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(self.data.len());
                let n = (self.data.len() - start).min(buf.len());
                buf[..n].copy_from_slice(&self.data[start..start + n]);
                Ok(n)
            }

            fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>, ReadAtError> {
                // Record the requested length WITHOUT allocating it, then serve
                // only the bytes actually available.
                self.max_len.fetch_max(len as u64, Ordering::Relaxed);
                let start = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(self.data.len());
                let end = start.saturating_add(len).min(self.data.len());
                Ok(self.data[start..end].to_vec())
            }
        }

        let max_len = StdArc::new(std::sync::atomic::AtomicU64::new(0));
        let payload = b"1 0 obj\n<< /Good 1 >>\nendobj\n".to_vec();
        let data = Data::new(PdfData::streamed(OverReportingLen {
            data: payload.clone(),
            max_len: max_len.clone(),
        }));

        // Size the window from offset 0 to the file-length sentinel (the
        // over-reported len) - exactly as `next_object_bound` does for the
        // highest-offset object.
        let window = data
            .object_window(0, u64::MAX)
            .expect("the chunked window read still succeeds");
        assert_eq!(window, payload, "the served bytes are returned unchanged");

        let observed = max_len.load(Ordering::Relaxed);
        assert!(
            observed <= CHUNK,
            "a window read must be issued in bounded chunks, never one giant \
             vec![0; want] sized from an over-reported ReadAt::len; a single read \
             requested {observed} bytes",
        );
    }

    /// Finding 1 (multi-chunk accumulation): `read_window` must accumulate a
    /// window that spans several chunks in full and stop exactly at `max_len`
    /// (the trusted next-object span), never truncating to one chunk nor
    /// over-reading to end-of-file. Both growth policies must return identical
    /// bytes; the amortised policy must additionally keep the buffer capped at
    /// the span (never the up-to-2x overshoot of unbounded doubling), so a
    /// legitimately large streamed object is not over-allocated.
    #[test]
    fn read_window_reads_multi_chunk_span_and_stops_at_bound() {
        const CHUNK: usize = 1 << 20;
        let total = 3 * CHUNK; // 3 MiB source, so the window spans chunks.
        let mut buf = vec![0_u8; total];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let data = PdfData::streamed(SliceSource(buf.clone()));

        // A trusted span of 2.5 MiB (< the 3 MiB file length): read across three
        // chunks, not truncated to one and not extended past the bound.
        let want = (2 * CHUNK + CHUNK / 2) as u64;
        for growth in [WindowGrowth::Amortised, WindowGrowth::Exact] {
            let window =
                read_window(&data, 0, want, growth).expect("the multi-chunk read succeeds");
            assert_eq!(
                window.as_slice(),
                &buf[..want as usize],
                "the window must be the exact [offset, offset + max_len) span ({growth:?})",
            );
            assert!(
                window.capacity() <= want as usize,
                "growth must stay capped at the span, never over-allocate past it \
                 ({growth:?}): capacity {} exceeds want {want}",
                window.capacity(),
            );
        }
    }

    /// Finding 1 (amortised growth is served-driven, not span-driven): the
    /// trusted object-window branch is UNCAPPED - `window_len` returns the raw
    /// next-object span, bounded only by the *untrusted* `ReadAt::len`, so a
    /// source that over-reports its length can place a distant next-object offset
    /// and make the span enormous. The amortised policy must therefore grow with
    /// the bytes actually served and NEVER reserve the span up front: reserving
    /// it would reintroduce exactly the memory-amplification the chunked read
    /// exists to prevent. A source serving one chunk then signalling end-of-input
    /// must yield a buffer whose capacity tracks the served bytes, not the span.
    #[test]
    fn read_window_amortised_growth_tracks_served_bytes_not_span() {
        const CHUNK: usize = 1 << 20;
        let served = vec![7_u8; CHUNK];
        let data = PdfData::streamed(SliceSource(served.clone()));

        // A large trusted span, but the source has only one chunk of real bytes.
        let want = 64 * CHUNK as u64; // 64 MiB span, 1 MiB served.
        let window =
            read_window(&data, 0, want, WindowGrowth::Amortised).expect("the short read succeeds");
        assert_eq!(
            window.as_slice(),
            served.as_slice(),
            "the window serves the bytes actually available",
        );
        assert!(
            window.capacity() <= 2 * CHUNK,
            "amortised growth must track the {} B served, never reserve the {} B \
             span up front (a memory-amplification DoS); capacity was {} B",
            served.len(),
            want,
            window.capacity(),
        );
    }
}
