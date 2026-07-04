//! Hint-table decoding for linearized PDF (ISO 32000-2 Annex F.4).
//!
//! The primary hint stream carries a *page offset hint table* (F.4.2,
//! Tables F.3/F.4) followed by a *shared object hint table* (F.4.3,
//! Tables F.5/F.6), located by the stream dictionary's byte offsets
//! (Table F.2). Both are decoded here on the crate's public
//! [`BitReader`]: F.4.1 specifies a high-order-bit-first bit stream,
//! exactly this reader's behaviour.
//!
//! # Column layout: byte-aligned in practice
//!
//! F.4.1 says fields are packed "without regard to byte boundaries" and
//! only each *table* "shall begin at a byte boundary". Real linearizers
//! (the `andler-optimal-lot-size` fixture is pdfTeX-produced) instead
//! begin *every* transposed column (F.4.2 items a–g, F.4.3 items a–d) on
//! a byte boundary. This is provable from the fixture's own metadata: the
//! page offset table decodes to occupy exactly `[0, /S)` and the shared
//! table exactly `[/S, len)`, its three page offsets land precisely on
//! the `/O`-object's and the following pages' cross-reference offsets, and
//! the first page's shared-reference count decodes to `0` as Table F.4
//! item 3 (§F.4.2) mandates — none of which hold under contiguous bit
//! packing. The decoder therefore
//! [`align`](crate::bit_reader::BitReader::align)s before each column.
//! Within a column, fields stay bit-packed.
//!
//! # `DoS` discipline (design §2.3)
//!
//! The decoded hint stream is a bounded buffer, but a zero-width delta
//! column consumes no bits, so an entry count taken at face value could
//! drive unbounded iteration. Counts are therefore capped
//! (`entry_cap`); no `Vec` is ever pre-sized from a hint-provided count;
//! and any field read past the buffer, or a bit-width field greater than
//! 32, aborts the whole decode to `None` (no partial tables surfaced).

use super::Linearization;
use crate::bit_reader::BitReader;
use alloc::vec::Vec;
use core::ops::Range;

/// Hard ceiling on materialised per-page / per-group entries, independent
/// of the buffer. A zero-width delta column self-limits by nothing, so an
/// entry count is clamped to this before iterating (design §2.3). Far
/// above any real document.
const MAX_HINT_ENTRIES: usize = 1 << 20;

/// The largest entry count worth materialising from a hint stream of the
/// given decoded length: the hard [`MAX_HINT_ENTRIES`] ceiling, further
/// bounded by the number of bits in the buffer (`decoded_len * 8`, `+1` so
/// a fully-consumed single-bit column is representable). Every per-entry
/// column reads at least zero bits, so this bounds total decode work to
/// `O(decoded_len)` even when every delta column is zero-width.
fn entry_cap(decoded_len: usize) -> usize {
    MAX_HINT_ENTRIES.min(decoded_len.saturating_mul(8).saturating_add(1))
}

/// Page offset hint table header — ISO 32000-2 Table F.3.
///
/// Every "number of bits needed" item is a 16-bit field holding a value in
/// `0..=32` (F.4.2); it is rejected during decoding if it exceeds 32.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageOffsetHeader {
    /// Item 1: the least number of objects in a page (including the page
    /// object itself).
    pub least_objects_per_page: u32,
    /// Item 2: the (hint-free) location of the first page's page object.
    pub first_page_location: u32,
    /// Item 3: bits for the per-page object-count delta.
    pub objects_delta_bits: u8,
    /// Item 4: the least length of a page in bytes.
    pub least_page_length: u32,
    /// Item 5: bits for the per-page page-length delta.
    pub page_length_delta_bits: u8,
    /// Item 6: the least offset of the start of any content stream,
    /// relative to the beginning of its page.
    pub least_content_offset: u32,
    /// Item 7: bits for the per-page content-offset delta.
    pub content_offset_delta_bits: u8,
    /// Item 8: the least content stream length.
    pub least_content_length: u32,
    /// Item 9: bits for the per-page content-length delta.
    pub content_length_delta_bits: u8,
    /// Item 10: bits for a page's shared-object reference count.
    pub shared_ref_count_bits: u8,
    /// Item 11: bits for a shared-object identifier.
    pub shared_id_bits: u8,
    /// Item 12: bits for a shared-reference fraction numerator.
    pub fraction_numerator_bits: u8,
    /// Item 13: the denominator of the fractional position shared by the
    /// whole document.
    pub fraction_denominator: u16,
}

/// A single shared-object reference from a page (Table F.4 items 4/5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SharedRef {
    /// Item 4: an index into the shared object hint table.
    pub shared_id: u32,
    /// Item 5: the numerator of the fractional position at which the page's
    /// content stream first references the shared object.
    pub fraction_numerator: u32,
}

/// A page offset hint table per-page entry — ISO 32000-2 Table F.4, with
/// each delta already added to its header least-value.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageEntry {
    /// Item 1: the number of objects in the page (`least_objects_per_page`
    /// plus the per-page delta).
    pub object_count: u32,
    /// Item 2: the length of the page in bytes (`least_page_length` plus the
    /// per-page delta).
    pub page_length: u32,
    /// Items 4/5: the shared objects this page references. Empty for a
    /// conforming first page (Table F.4 item 3).
    pub shared_refs: Vec<SharedRef>,
    /// Item 6: the offset of the start of the page's content stream,
    /// relative to the page (`least_content_offset` plus the delta).
    pub content_offset: u32,
    /// Item 7: the length of the page's content stream
    /// (`least_content_length` plus the delta).
    pub content_length: u32,
}

/// A decoded page offset hint table (F.4.2): the header plus one entry per
/// page, in page order starting with the first page.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageOffsetTable {
    /// The header section (Table F.3).
    pub header: PageOffsetHeader,
    /// One entry per page (Table F.4).
    pub pages: Vec<PageEntry>,
}

/// Shared object hint table header — ISO 32000-2 Table F.5.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SharedObjectHeader {
    /// Item 1: the object number of the first object in the shared objects
    /// section (part 8).
    pub shared_section_first_object: u32,
    /// Item 2: the (hint-free) location of the first object in the shared
    /// objects section.
    pub shared_section_location: u32,
    /// Item 3: the number of shared object entries for the first page
    /// (including nonshared objects, F.4.3).
    pub first_page_entry_count: u32,
    /// Item 4: the total number of shared object entries, *including* the
    /// first-page entries of item 3.
    pub total_entry_count: u32,
    /// Item 5: bits for the greatest number of objects in a group.
    pub group_objects_bits: u8,
    /// Item 6: the least length of a shared object group in bytes.
    pub least_group_length: u32,
    /// Item 7: bits for the per-group length delta.
    pub group_length_delta_bits: u8,
}

/// A shared object hint table group entry — ISO 32000-2 Table F.6.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SharedGroup {
    /// Item 1: the length of the object group in bytes (`least_group_length`
    /// plus the per-group delta).
    pub group_length: u32,
    /// Item 3: the group's 16-byte MD5 signature, present only when the
    /// item-2 flag is set.
    pub signature: Option<[u8; 16]>,
    /// Item 4: the number of objects in the group (the encoded value plus
    /// one).
    pub object_count: u32,
}

/// A decoded shared object hint table (F.4.3): the header plus the
/// first-page group sequence followed by the shared-objects-section group
/// sequence, both stored here in order.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SharedObjectTable {
    /// The header section (Table F.5).
    pub header: SharedObjectHeader,
    /// The group entries (Table F.6): the first-page groups
    /// ([`SharedObjectHeader::first_page_entry_count`] of them) followed by
    /// the shared-section groups.
    pub groups: Vec<SharedGroup>,
}

/// The decoded hint tables of a linearized PDF: the required page offset
/// and shared object hint tables (ISO 32000-2 F.4.2/F.4.3).
///
/// Obtain via [`Pdf::hint_tables`](crate::Pdf::hint_tables).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HintTables {
    /// The page offset hint table (F.4.2).
    pub page_offset: PageOffsetTable,
    /// The shared object hint table (F.4.3).
    pub shared: SharedObjectTable,
}

/// Read a bit field of the given width (F.4.1). The spec permits a width of
/// 0 ("values in the range 0 through 32"): a zero-width field is always 0
/// and consumes no bits. [`BitReader::read`] rejects a width below 1 (and
/// above 32), so that case is handled here; a width above 32 propagates as
/// `None`, aborting the decode.
fn read_field(reader: &mut BitReader<'_>, bits: u8) -> Option<u32> {
    if bits == 0 {
        Some(0)
    } else {
        reader.read(bits)
    }
}

/// Read a 16-bit "number of bits needed" header field (F.4.2: such items
/// hold a value in `0..=32` but are stored in 16 bits). A value above 32 is
/// invalid as a later field width, so it aborts the decode (`None`).
fn read_width(reader: &mut BitReader<'_>) -> Option<u8> {
    let value = reader.read(16)?;
    if value > 32 {
        return None;
    }
    // Fits u8: guarded above.
    u8::try_from(value).ok()
}

/// Read a plain 16-bit value (a non-width header field, e.g. the fraction
/// denominator, Table F.3 item 13).
fn read_u16(reader: &mut BitReader<'_>) -> Option<u16> {
    u16::try_from(reader.read(16)?).ok()
}

/// Read a 128-bit big-endian value as 16 bytes: the shared-object MD5
/// signature (Table F.6 item 3), assembled from four high-order-bit-first
/// 32-bit reads (F.4.1).
fn read_u128_be(reader: &mut BitReader<'_>) -> Option<[u8; 16]> {
    let mut out = [0_u8; 16];
    for word in out.chunks_mut(4) {
        word.copy_from_slice(&reader.read(32)?.to_be_bytes());
    }
    Some(out)
}

/// Align to the next byte boundary (each column begins byte-aligned; see
/// the module note) and read `count` fields of `bits` width.
fn read_column(reader: &mut BitReader<'_>, bits: u8, count: usize) -> Option<Vec<u32>> {
    reader.align();
    let mut out = Vec::new();
    for _ in 0..count {
        out.push(read_field(reader, bits)?);
    }
    Some(out)
}

fn read_page_offset_header(reader: &mut BitReader<'_>) -> Option<PageOffsetHeader> {
    // Read strictly in Table F.3 item order 1..=13; every read advances the
    // bit cursor, so the sequence must not be reordered.
    let least_objects_per_page = reader.read(32)?;
    let first_page_location = reader.read(32)?;
    let objects_delta_bits = read_width(reader)?;
    let least_page_length = reader.read(32)?;
    let page_length_delta_bits = read_width(reader)?;
    let least_content_offset = reader.read(32)?;
    let content_offset_delta_bits = read_width(reader)?;
    let least_content_length = reader.read(32)?;
    let content_length_delta_bits = read_width(reader)?;
    let shared_ref_count_bits = read_width(reader)?;
    let shared_id_bits = read_width(reader)?;
    let fraction_numerator_bits = read_width(reader)?;
    let fraction_denominator = read_u16(reader)?;
    Some(PageOffsetHeader {
        least_objects_per_page,
        first_page_location,
        objects_delta_bits,
        least_page_length,
        page_length_delta_bits,
        least_content_offset,
        content_offset_delta_bits,
        least_content_length,
        content_length_delta_bits,
        shared_ref_count_bits,
        shared_id_bits,
        fraction_numerator_bits,
        fraction_denominator,
    })
}

/// Decode the page offset hint table (F.4.2). `pages` is the page count
/// (already capped by the caller); `ref_cap` bounds the total shared
/// references (design §2.3).
fn parse_page_offset_table(
    reader: &mut BitReader<'_>,
    pages: usize,
    ref_cap: usize,
) -> Option<PageOffsetTable> {
    let header = read_page_offset_header(reader)?;
    let n = pages;

    // Transposed columns, F.4.2 order a–g, each byte-aligned (module note).
    // a) item 1: per-page object-count delta.
    let object_deltas = read_column(reader, header.objects_delta_bits, n)?;
    // b) item 2: per-page page-length delta.
    let page_length_deltas = read_column(reader, header.page_length_delta_bits, n)?;
    // c) item 3: per-page shared-reference count. The first page's count is 0
    // (Table F.4 item 3); it is still encoded, so it is read like any other
    // page.
    let shared_ref_counts = read_column(reader, header.shared_ref_count_bits, n)?;

    // Bound the total shared references before reading the per-reference
    // columns d/e: a zero-width id or numerator column would otherwise let a
    // hostile count column drive unbounded iteration (design §2.3).
    let mut total_refs = 0_usize;
    for &count in &shared_ref_counts {
        total_refs = total_refs.checked_add(count as usize)?;
        if total_refs > ref_cap {
            return None;
        }
    }

    // d) item 4: shared-object identifiers for pages 2..n, in page order (the
    // first page contributes none). Byte-aligned block, then bit-packed.
    reader.align();
    let mut shared_ids = Vec::new();
    for &count in &shared_ref_counts {
        for _ in 0..count {
            shared_ids.push(read_field(reader, header.shared_id_bits)?);
        }
    }
    // e) item 5: fraction numerators, same order and counts.
    reader.align();
    let mut numerators = Vec::new();
    for &count in &shared_ref_counts {
        for _ in 0..count {
            numerators.push(read_field(reader, header.fraction_numerator_bits)?);
        }
    }
    // f) item 6: per-page content-offset delta.
    let content_offset_deltas = read_column(reader, header.content_offset_delta_bits, n)?;
    // g) item 7: per-page content-length delta.
    let content_length_deltas = read_column(reader, header.content_length_delta_bits, n)?;

    // Assemble per-page entries, re-associating the flat id/numerator columns
    // with each page by its shared-reference count.
    let mut ids = shared_ids.into_iter();
    let mut nums = numerators.into_iter();
    let entries = object_deltas
        .iter()
        .zip(&page_length_deltas)
        .zip(&shared_ref_counts)
        .zip(&content_offset_deltas)
        .zip(&content_length_deltas)
        .map(|((((obj_d, len_d), ref_count), coff_d), clen_d)| {
            let mut shared_refs = Vec::new();
            for _ in 0..*ref_count {
                shared_refs.push(SharedRef {
                    shared_id: ids.next()?,
                    fraction_numerator: nums.next()?,
                });
            }
            Some(PageEntry {
                object_count: header.least_objects_per_page.wrapping_add(*obj_d),
                page_length: header.least_page_length.wrapping_add(*len_d),
                shared_refs,
                content_offset: header.least_content_offset.wrapping_add(*coff_d),
                content_length: header.least_content_length.wrapping_add(*clen_d),
            })
        })
        .collect::<Option<Vec<_>>>()?;

    Some(PageOffsetTable {
        header,
        pages: entries,
    })
}

fn read_shared_object_header(reader: &mut BitReader<'_>) -> Option<SharedObjectHeader> {
    // Table F.5 item order 1..=7; do not reorder (each read advances bits).
    let shared_section_first_object = reader.read(32)?;
    let shared_section_location = reader.read(32)?;
    let first_page_entry_count = reader.read(32)?;
    let total_entry_count = reader.read(32)?;
    let group_objects_bits = read_width(reader)?;
    let least_group_length = reader.read(32)?;
    let group_length_delta_bits = read_width(reader)?;
    Some(SharedObjectHeader {
        shared_section_first_object,
        shared_section_location,
        first_page_entry_count,
        total_entry_count,
        group_objects_bits,
        least_group_length,
        group_length_delta_bits,
    })
}

/// Decode one column-major block of `count` shared object group entries
/// (F.4.3 items a–d), each column byte-aligned (module note).
fn read_shared_group_block(
    reader: &mut BitReader<'_>,
    header: &SharedObjectHeader,
    count: usize,
) -> Option<Vec<SharedGroup>> {
    // a) item 1: per-group length delta.
    let length_deltas = read_column(reader, header.group_length_delta_bits, count)?;
    // b) item 2: per-group signature-present flag (1 bit).
    reader.align();
    let mut flags = Vec::new();
    for _ in 0..count {
        flags.push(reader.read(1)? == 1);
    }
    // c) item 3: 128-bit signature, only for flagged groups.
    reader.align();
    let mut signatures = Vec::new();
    for &present in &flags {
        signatures.push(if present {
            Some(read_u128_be(reader)?)
        } else {
            None
        });
    }
    // d) item 4: per-group object count minus one.
    let count_minus_ones = read_column(reader, header.group_objects_bits, count)?;

    let groups = length_deltas
        .into_iter()
        .zip(signatures)
        .zip(count_minus_ones)
        .map(|((len_d, signature), count_minus_one)| SharedGroup {
            group_length: header.least_group_length.wrapping_add(len_d),
            signature,
            object_count: count_minus_one.wrapping_add(1),
        })
        .collect();
    Some(groups)
}

/// Decode the shared object hint table (F.4.3). `cap` bounds each group
/// count (design §2.3).
fn parse_shared_object_table(reader: &mut BitReader<'_>, cap: usize) -> Option<SharedObjectTable> {
    let header = read_shared_object_header(reader)?;
    let total = (header.total_entry_count as usize).min(cap);

    // F.4.3: the shared object group entries are a single column-major block
    // a–d over all `total_entry_count` groups. Item 4 counts the first-page
    // entries of item 3 among the total, so the leading `first_page_entry_count`
    // groups describe the first page and the remainder the shared-objects
    // section — but both are transposed together, not as two independent
    // blocks. (Empirical, design §7.7: the `font_truetype_slow_post_lookup`
    // fixture has a non-empty section, and a two-block read overruns the
    // table's byte span and aborts the decode; the `andler` fixture cannot
    // distinguish the two, its section being empty.)
    let groups = read_shared_group_block(reader, &header, total)?;

    Some(SharedObjectTable { header, groups })
}

/// F.4.1 offset adjustment: a hint-table position at or beyond the primary
/// hint stream's offset is shifted later by the stream's length, since hint
/// positions are expressed "as if the primary hint stream itself were not
/// present".
///
/// The spec says a position *greater than* the offset is shifted, but the
/// boundary is inclusive: the first object physically after the hint stream
/// occupies the stream's own start position in the hint-free coordinate
/// system, so its hint-free location equals `off1` and it must still be
/// shifted. On the fixture the `/O` page object's hint-free location is
/// exactly `off1` (83139) and must resolve to its real offset 83298 =
/// `off1 + len1`; a strict `>` would leave it at 83139. Applies to hint
/// offsets only — never to `/T`, `/E`, `/O`'s cross-reference entry, or the
/// linearization dict (F.4.1).
fn adjust(pos: u64, off1: u64, len1: u64) -> u64 {
    if pos >= off1 {
        pos.saturating_add(len1)
    } else {
        pos
    }
}

/// The primary hint stream's `(offset, length)` = `/H[0..2]` (Table F.1),
/// the parameters of the F.4.1 offset [`adjust`]ment.
fn hint_stream_offset_len(lin: &Linearization) -> Option<(u64, u64)> {
    let offsets = lin.hint_offsets.as_ref()?;
    let off1 = u64::try_from(*offsets.first()?).ok()?;
    let len1 = u64::try_from(*offsets.get(1)?).ok()?;
    Some((off1, len1))
}

impl HintTables {
    /// Decode the page offset and shared object hint tables from the
    /// (concatenated, decoded) hint stream `data`. `shared_table_offset` is
    /// the `/S` byte offset of the shared table within `data` (Table F.2);
    /// `page_count` is the document's page count (`/N`).
    ///
    /// Returns `None` if either table cannot be fully decoded (a field read
    /// past the buffer, or an out-of-range bit width), so a caller never
    /// sees a partially-decoded table.
    pub(crate) fn decode(
        data: &[u8],
        shared_table_offset: usize,
        page_count: usize,
    ) -> Option<Self> {
        let cap = entry_cap(data.len());
        let pages = page_count.min(cap);

        // The page offset hint table is first and starts at offset 0 (F.4.1).
        let mut page_reader = BitReader::new(data);
        let page_offset = parse_page_offset_table(&mut page_reader, pages, cap)?;

        // The shared object hint table starts at the /S byte offset; each hint
        // table begins on a byte boundary (F.4.1), so a fresh reader over the
        // tail slice is already aligned.
        let shared_slice = data.get(shared_table_offset..)?;
        let mut shared_reader = BitReader::new(shared_slice);
        let shared = parse_shared_object_table(&mut shared_reader, cap)?;

        Some(Self {
            page_offset,
            shared,
        })
    }

    /// The object number of the first object of the page at `page_index`
    /// (Table F.4 item 1).
    ///
    /// The first page's first object is `/O` (`lin.first_page_object`); the
    /// second page's is object 1; each later page's is found by accumulating
    /// the object counts of the pages before it. Returns `None` if
    /// `page_index` is out of range or the accumulated number overflows.
    pub fn page_object_number(&self, page_index: usize, lin: &Linearization) -> Option<i32> {
        let pages = &self.page_offset.pages;
        if page_index >= pages.len() {
            return None;
        }
        if page_index == 0 {
            return Some(lin.first_page_object);
        }
        // Page 1 (index 1) starts at object 1; each subsequent page adds the
        // object counts of the pages strictly between page 1 and it.
        let number = pages[1..page_index]
            .iter()
            .try_fold(1_u32, |acc, page| acc.checked_add(page.object_count))?;
        i32::try_from(number).ok()
    }

    /// The byte range `[start, end)` occupied by the page at `page_index`,
    /// relative to the beginning of the file (Table F.4 item 2).
    ///
    /// The first page starts at the header's `first_page_location` and each
    /// later page starts after accumulating all previous pages' lengths; the
    /// accumulated position is then shifted past the primary hint stream per
    /// the F.4.1 `adjust`ment. The end is the start plus the page's own
    /// length, so `page_byte_range(0).end` equals `/E`
    /// (`lin.first_page_end_offset`). Returns `None` if `page_index` is out
    /// of range, `/H` is unavailable, or an accumulation overflows.
    pub fn page_byte_range(&self, page_index: usize, lin: &Linearization) -> Option<Range<u64>> {
        let pages = &self.page_offset.pages;
        let target = pages.get(page_index)?;
        let (off1, len1) = hint_stream_offset_len(lin)?;

        // Accumulate all previous pages' lengths in the hint-free coordinate
        // system, then adjust once (F.4.1).
        let mut raw_start = u64::from(self.page_offset.header.first_page_location);
        for page in &pages[..page_index] {
            raw_start = raw_start.checked_add(u64::from(page.page_length))?;
        }
        let start = adjust(raw_start, off1, len1);
        let end = start.checked_add(u64::from(target.page_length))?;
        Some(start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bit_reader::BitWriter;

    /// Append a byte-aligned column of `count` values, each `bits` wide, to
    /// `buf` (matching the decoder's per-column alignment). A zero-width
    /// column contributes no bytes and leaves the buffer byte-aligned.
    fn push_column(buf: &mut Vec<u8>, bits: u8, values: &[u32]) {
        if bits == 0 {
            return;
        }
        let start = buf.len();
        let nbytes = (bits as usize * values.len()).div_ceil(8);
        buf.resize(start + nbytes, 0);
        let mut writer = BitWriter::new(&mut buf[start..], bits).unwrap();
        for &value in values {
            writer.write(value).unwrap();
        }
    }

    fn push_u32(buf: &mut Vec<u8>, value: u32) {
        buf.extend_from_slice(&value.to_be_bytes());
    }

    fn push_u16(buf: &mut Vec<u8>, value: u16) {
        buf.extend_from_slice(&value.to_be_bytes());
    }

    /// A synthetic two-page page offset table and a three-group shared
    /// object table, hand-encoded with byte-aligned columns and decoded back
    /// exactly. Exercises `read_field(0)` (the zero-width content-offset and
    /// numerator columns), `read_u128_be` (two signature-present groups),
    /// and the single-block shared transposition over all 3 groups
    /// (`first_page_entry_count` 2 + a 1-group shared section).
    #[test]
    fn round_trip_synthetic_tables() {
        // --- Page offset table (Table F.3 header + F.4 entries) ---
        let mut buf = Vec::new();
        // Header items 1..=13.
        push_u32(&mut buf, 10); // 1 least objects/page
        push_u32(&mut buf, 500); // 2 first-page location
        push_u16(&mut buf, 4); // 3 objects delta bits
        push_u32(&mut buf, 100); // 4 least page length
        push_u16(&mut buf, 8); // 5 page-length delta bits
        push_u32(&mut buf, 0); // 6 least content offset
        push_u16(&mut buf, 0); // 7 content-offset delta bits (ZERO WIDTH)
        push_u32(&mut buf, 50); // 8 least content length
        push_u16(&mut buf, 4); // 9 content-length delta bits
        push_u16(&mut buf, 2); // 10 shared-ref-count bits
        push_u16(&mut buf, 5); // 11 shared-id bits
        push_u16(&mut buf, 0); // 12 fraction-numerator bits (ZERO WIDTH)
        push_u16(&mut buf, 4); // 13 fraction denominator
        // Columns a–g for 2 pages.
        push_column(&mut buf, 4, &[2, 3]); // a: object deltas -> counts 12, 13
        push_column(&mut buf, 8, &[10, 20]); // b: page-length deltas -> 110, 120
        push_column(&mut buf, 2, &[0, 2]); // c: shared-ref counts (page 0 = 0)
        push_column(&mut buf, 5, &[7, 9]); // d: shared ids for page 1's 2 refs
        // e: numerators are zero-width -> no bytes.
        push_column(&mut buf, 0, &[0, 0]);
        // f: content-offset deltas are zero-width -> no bytes.
        push_column(&mut buf, 0, &[0, 0]);
        push_column(&mut buf, 4, &[5, 6]); // g: content-length deltas -> 55, 56

        let shared_offset = buf.len();

        // --- Shared object table (Table F.5 header + F.6 groups) ---
        push_u32(&mut buf, 1); // 1 shared-section first object
        push_u32(&mut buf, 1000); // 2 shared-section location
        push_u32(&mut buf, 2); // 3 first-page entry count
        push_u32(&mut buf, 3); // 4 total entry count (2 first-page + 1 section)
        push_u16(&mut buf, 3); // 5 group-objects bits
        push_u32(&mut buf, 10); // 6 least group length
        push_u16(&mut buf, 8); // 7 group-length delta bits

        let sig_a = [0xAA; 16];
        let sig_b = [0xBB; 16];
        // Single column-major block over all 3 groups (first-page 2 + section 1).
        push_column(&mut buf, 8, &[5, 7, 3]); // a) length deltas -> 15, 17, 13
        push_column(&mut buf, 1, &[1, 0, 1]); // b) signature-present flags
        buf.extend_from_slice(&sig_a); // c) signatures for the flagged groups 0
        buf.extend_from_slice(&sig_b); //    and 2, in group order
        push_column(&mut buf, 3, &[0, 2, 1]); // d) object-count-minus-one -> 1, 3, 2

        // --- Decode and assert ---
        let tables = HintTables::decode(&buf, shared_offset, 2).expect("synthetic decode");
        let po = &tables.page_offset;
        assert_eq!(po.header.least_objects_per_page, 10);
        assert_eq!(po.header.first_page_location, 500);
        assert_eq!(po.header.content_offset_delta_bits, 0);
        assert_eq!(po.header.fraction_numerator_bits, 0);
        assert_eq!(po.header.fraction_denominator, 4);
        assert_eq!(po.pages.len(), 2);

        assert_eq!(po.pages[0].object_count, 12);
        assert_eq!(po.pages[0].page_length, 110);
        assert!(po.pages[0].shared_refs.is_empty());
        assert_eq!(po.pages[0].content_offset, 0);
        assert_eq!(po.pages[0].content_length, 55);

        assert_eq!(po.pages[1].object_count, 13);
        assert_eq!(po.pages[1].page_length, 120);
        assert_eq!(
            po.pages[1].shared_refs,
            alloc::vec![
                SharedRef {
                    shared_id: 7,
                    fraction_numerator: 0,
                },
                SharedRef {
                    shared_id: 9,
                    fraction_numerator: 0,
                },
            ],
        );
        assert_eq!(po.pages[1].content_length, 56);

        let sh = &tables.shared;
        assert_eq!(sh.header.first_page_entry_count, 2);
        assert_eq!(sh.header.total_entry_count, 3);
        assert_eq!(sh.groups.len(), 3);
        assert_eq!(sh.groups[0].group_length, 15);
        assert_eq!(sh.groups[0].signature, Some(sig_a));
        assert_eq!(sh.groups[0].object_count, 1);
        assert_eq!(sh.groups[1].group_length, 17);
        assert_eq!(sh.groups[1].signature, None);
        assert_eq!(sh.groups[1].object_count, 3);
        assert_eq!(sh.groups[2].group_length, 13);
        assert_eq!(sh.groups[2].signature, Some(sig_b));
        assert_eq!(sh.groups[2].object_count, 2);
    }

    /// A width field above 32 (invalid as a later field width, F.4.2) aborts
    /// the decode rather than being used.
    #[test]
    fn over_wide_width_field_aborts() {
        let mut buf = Vec::new();
        push_u32(&mut buf, 0); // 1 least objects/page
        push_u32(&mut buf, 0); // 2 first-page location
        push_u16(&mut buf, 33); // 3 objects delta bits -> INVALID (> 32)
        // Remaining header bytes are irrelevant; pad so the header read itself
        // does not run short before reaching the invalid width.
        buf.resize(64, 0);
        assert!(HintTables::decode(&buf, 40, 1).is_none());
    }

    /// A truncated buffer (a field read runs past the end) aborts to `None`,
    /// surfacing no partial table.
    #[test]
    fn truncated_buffer_aborts() {
        // Only a few bytes: the 32-bit header reads exhaust the buffer.
        let buf = [0_u8; 3];
        assert!(HintTables::decode(&buf, 0, 1).is_none());
    }
}
