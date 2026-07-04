//! A positioned-read byte source for streaming PDF parsing.
//!
//! PDF is a random-access format: the cross-reference table maps object
//! numbers to absolute byte offsets, so a parser never needs the whole
//! file resident — it can read the trailer and xref with bounded tail
//! reads, then read each accessed object's byte range on demand. The
//! [`ReadAt`] trait is that on-demand source.
//!
//! A [`ReadAt`] source is the streaming counterpart of the resident
//! [`crate::PdfData`] byte buffer: pass one to [`crate::Pdf::new_with_reader`]
//! to parse a document without buffering the entire file. Only the objects
//! actually touched are materialised (each cached for the document's
//! lifetime); a malformed file whose cross-reference table must be rebuilt
//! by brute force falls back to a full read (see [`crate::Pdf`]).
//!
//! # `no_std`
//!
//! The trait is expressed in `core` + `alloc` only — it deliberately does
//! **not** use `std::io::Read`/`Seek`, so it works in the crate's `no_std`
//! configuration and on WASM, where a host supplies bytes synchronously.

use alloc::vec;
use alloc::vec::Vec;

/// An error raised by a [`ReadAt`] source.
///
/// Deliberately coarse: the parser distinguishes a hard source failure
/// ([`Io`](ReadAtError::Io)) and a "not yet, retry later" signal for a
/// partial download ([`Pending`](ReadAtError::Pending)) from a clean
/// end-of-input, which is signalled by a short read rather than an error.
/// The enum is `#[non_exhaustive]` so future variants do not break callers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadAtError {
    /// The underlying source reported an I/O failure.
    Io,
    /// The requested bytes lie within the source's reported length but are
    /// not yet available — a partially-downloaded Fast Web View prefix
    /// (ISO 32000-2 Annex F, §G.2) that has not reached this range. Unlike
    /// [`Io`](ReadAtError::Io) this is transient: a streaming consumer
    /// should fetch more of the source and retry the same read rather than
    /// treat the document as broken. A short read still means a clean
    /// end-of-source; `Pending` is reserved for bytes genuinely
    /// absent-for-now, so nothing derived from it is cached and a later
    /// read re-issues.
    Pending,
}

impl core::fmt::Display for ReadAtError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io => f.write_str("positioned-read source I/O error"),
            Self::Pending => f.write_str("positioned-read source bytes not yet available"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ReadAtError {}

/// A source of PDF bytes addressable by absolute byte offset.
///
/// # Contract
///
/// * [`read_at`](ReadAt::read_at) takes `&self`, never `&mut self`. The
///   parser issues nested / re-entrant reads — resolving an indirect
///   `/Length` triggers a second positioned read while the stream-body
///   read is still in flight — and under `no_std` the parser holds its
///   shared state behind `Rc` + `RefCell`, where a `&mut self` source
///   would provoke a re-entrant borrow panic. Any buffering or read
///   accounting an implementor keeps must therefore live behind interior
///   mutability.
/// * A read of fewer bytes than requested (a *short read*) means the end
///   of the source was reached at that point; `0` bytes means
///   `offset >= len()`. A short read is **not** an error — only a genuine
///   source failure returns [`ReadAtError`].
/// * Reads must be idempotent and side-effect-free with respect to the
///   bytes returned: the parser re-reads the same range freely (the
///   transactional rewind-on-parse-failure pattern depends on it).
///
/// Under the `std` feature the parser stores the source as
/// `Arc<dyn ReadAt + Send + Sync>`, so a source used with `std` must be
/// `Send + Sync`; under `no_std` the crate is single-threaded and no such
/// bound applies.
pub trait ReadAt {
    /// The total length of the source in bytes.
    fn len(&self) -> u64;

    /// Whether the source is empty (`len() == 0`).
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read into `buf` starting at absolute byte `offset`, returning the
    /// number of bytes read.
    ///
    /// May read fewer bytes than `buf.len()` only when the end of the
    /// source is reached; returns `0` when `offset >= len()`. Returns
    /// [`ReadAtError::Io`] on a genuine source failure, or
    /// [`ReadAtError::Pending`] when the requested bytes lie within
    /// [`len`](ReadAt::len) but are not yet available in a partial
    /// download (the caller retries after more of the source lands).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadAtError>;

    /// Read repeatedly from `offset` until `buf` is full or the end of the
    /// source is reached, coalescing the short reads [`read_at`](ReadAt::read_at)
    /// is permitted to return. Returns the number of bytes filled, which is
    /// less than `buf.len()` only at end-of-source.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadAtError> {
        let mut filled = 0_usize;
        while filled < buf.len() {
            match self.read_at(offset + filled as u64, &mut buf[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        Ok(filled)
    }

    /// Read the byte range `[offset, offset + len)` into a freshly
    /// allocated `Vec`, truncated to the bytes actually available at
    /// end-of-source.
    fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>, ReadAtError> {
        let mut buf = vec![0_u8; len];
        let n = self.read_exact_at(offset, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}
