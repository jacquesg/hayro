//! Streams.

use crate::crypto::DecryptionTarget;
use crate::filter::Filter;
use crate::object;
use crate::object::Dict;
use crate::object::Name;
use crate::object::dict::keys::{DECODE_PARMS, DP, F, FILTER, LENGTH, TYPE};
use crate::object::{Array, ObjectIdentifier};
use crate::object::{Object, ObjectLike, ObjectRefLike};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::trivia::is_white_space_character;
use crate::util::{OptionLog, find_needle};
use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::fmt::{Debug, Formatter};
use smallvec::SmallVec;

struct FiltersAndParams<'a> {
    filters: SmallVec<[Filter; 2]>,
    params: SmallVec<[Dict<'a>; 2]>,
}

/// A stream of arbitrary data.
#[derive(Clone)]
pub struct Stream<'a> {
    dict: Dict<'a>,
    data: &'a [u8],
}

impl PartialEq for Stream<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.dict == other.dict && self.data == other.data
    }
}

/// Additional parameters for decoding images.
#[derive(Clone, PartialEq, Default)]
pub struct ImageDecodeParams {
    /// Whether the color space of the image is an indexed color space.
    pub is_indexed: bool,
    /// The bits per component of the image, if that information is available.
    pub bpc: Option<u8>,
    /// The components per channel of the image, if that information is available.
    pub num_components: Option<u8>,
    /// A target resolution for the image. Note that this is only a hint so that
    /// in case it's possible, a version of the image will be extracted that
    /// is as close as possible to the hinted dimension.
    pub target_dimension: Option<(u32, u32)>,
    /// The width of the image as indicated by the image dictionary.
    pub width: u32,
    /// The height of the image as indicated by the image dictionary.
    pub height: u32,
}

impl<'a> Stream<'a> {
    pub(crate) fn new(data: &'a [u8], dict: Dict<'a>) -> Self {
        Self { dict, data }
    }

    fn filters_and_params(&self) -> FiltersAndParams<'a> {
        let mut collected_filters = SmallVec::new();
        let mut collected_params = SmallVec::new();

        if let Some(filter) = self
            .dict
            .get::<Name<'_>>(F)
            .or_else(|| self.dict.get::<Name<'_>>(FILTER))
            .and_then(Filter::from_name)
        {
            let params = self
                .dict
                .get::<Dict<'_>>(DP)
                .or_else(|| self.dict.get::<Dict<'_>>(DECODE_PARMS))
                .unwrap_or_default();

            collected_filters.push(filter);
            collected_params.push(params);
        } else if let Some(filters) = self
            .dict
            .get::<Array<'_>>(F)
            .or_else(|| self.dict.get::<Array<'_>>(FILTER))
        {
            let filters = filters.iter::<Name<'_>>().map(Filter::from_name);
            let mut params = self
                .dict
                .get::<Array<'_>>(DP)
                .or_else(|| self.dict.get::<Array<'_>>(DECODE_PARMS))
                .map(|a| a.iter::<Object<'_>>());

            for filter in filters {
                let params = params
                    .as_mut()
                    .and_then(|p| p.next())
                    .and_then(|p| p.into_dict())
                    .unwrap_or_default();

                if let Some(filter) = filter {
                    collected_filters.push(filter);
                    collected_params.push(params);
                }
            }
        }

        FiltersAndParams {
            filters: collected_filters,
            params: collected_params,
        }
    }

    /// Return the raw, decrypted data of the stream.
    ///
    /// Stream filters will not be applied.
    pub fn raw_data(&self) -> Cow<'a, [u8]> {
        let ctx = self.dict.ctx();

        if ctx.xref().needs_decryption(ctx)
            && self
                .dict
                .get::<object::String<'_>>(TYPE)
                .map(|t| t.as_ref() != b"XRef")
                .unwrap_or(true)
        {
            Cow::Owned(
                ctx.xref()
                    .decrypt(
                        self.dict.obj_id().unwrap(),
                        self.data,
                        DecryptionTarget::Stream,
                    )
                    // TODO: MAybe an error would be better?
                    .unwrap_or_default(),
            )
        } else {
            Cow::Borrowed(self.data)
        }
    }

    /// Return the raw, underlying dictionary of the stream.
    pub fn dict(&self) -> &Dict<'a> {
        &self.dict
    }

    /// Return the object identifier of the stream.
    pub fn obj_id(&self) -> ObjectIdentifier {
        self.dict.obj_id().unwrap()
    }

    /// Return the filters that are applied to the stream.
    pub fn filters(&self) -> SmallVec<[Filter; 2]> {
        self.filters_and_params().filters
    }

    /// Return the byte range of the stream body in the original PDF
    /// source bytes.
    ///
    /// The range covers the **raw encoded** body — i.e. bytes as stored
    /// on disk, including any Flate/ASCII85/etc. encoding. This is NOT
    /// the bytes returned by [`Self::decoded`].
    ///
    /// Returns `None` when the body is not contiguous in the source
    /// PDF (for example when the [`Stream`] was synthesised from
    /// content outside the main `Arc<Data>`, as happens with inline
    /// images constructed by the content-stream parser).
    ///
    /// Requires the `inspect` feature.
    #[cfg(feature = "inspect")]
    pub fn body_range(&self) -> Option<core::ops::Range<usize>> {
        let ctx = self.dict.ctx();
        let pdf_data = ctx.xref().data_bytes()?;

        let pdf_start = pdf_data.as_ptr() as usize;
        let pdf_end = pdf_start.wrapping_add(pdf_data.len());
        let body_start_addr = self.data.as_ptr() as usize;
        let body_end_addr = body_start_addr.wrapping_add(self.data.len());

        if body_start_addr < pdf_start
            || body_end_addr > pdf_end
            || body_end_addr < body_start_addr
        {
            return None;
        }

        let body_start = body_start_addr - pdf_start;
        let range = body_start..body_start + self.data.len();

        // Sanity: verify the bytes actually match, to guard against the
        // rare case where a separately-allocated slice happens to fall
        // within the PDF's address range.
        if pdf_data.get(range.clone())? != self.data {
            return None;
        }

        Some(range)
    }

    /// Return the byte range of the stream body if and only if no
    /// reversing filters apply.
    ///
    /// When the filter chain is empty or consists only of `/Identity`
    /// (no real decoding), the decoded bytes are bytewise identical to
    /// the encoded body, and a consumer can map a decoded-buffer offset
    /// to a file offset via `unfiltered_body_range()?.start + decoded_offset`.
    ///
    /// Returns `None` when any real filter is present (`/FlateDecode`,
    /// `/LZWDecode`, `/DCTDecode`, etc.) — reversing such filters is
    /// not generally possible and hayro does not attempt it. Also
    /// returns `None` whenever [`body_range`](Self::body_range) would.
    ///
    /// Requires the `inspect` feature.
    #[cfg(feature = "inspect")]
    pub fn unfiltered_body_range(&self) -> Option<core::ops::Range<usize>> {
        if self.filters().is_empty() {
            self.body_range()
        } else {
            None
        }
    }

    /// Return the physical layout of this stream's `stream`/`endstream`
    /// keyword pair.
    ///
    /// Computed on demand by scanning outward from
    /// [`body_range`](Self::body_range). Returns `None` whenever
    /// `body_range` would, or when the keywords cannot be located
    /// where the parser recorded them.
    ///
    /// Requires the `inspect` feature.
    #[cfg(feature = "inspect")]
    pub fn keyword_layout(&self) -> Option<StreamKeywordLayout> {
        let range = self.body_range()?;
        let pdf_data = self.dict.ctx().xref().data_bytes()?;

        // Identify the EOL immediately preceding the body.
        let before_body = pdf_data.get(..range.start)?;
        let (eol_size, stream_keyword_eol_canonical) =
            if before_body.ends_with(b"\r\n") {
                (2_usize, true)
            } else if before_body.ends_with(b"\n") {
                (1_usize, true)
            } else if before_body.ends_with(b"\r") {
                // CR alone is not permitted per §7.3.8.1.
                (1_usize, false)
            } else {
                return None;
            };

        let stream_keyword_end = range.start.checked_sub(eol_size)?;
        let stream_keyword_offset = stream_keyword_end.checked_sub(b"stream".len())?;
        if pdf_data.get(stream_keyword_offset..stream_keyword_end) != Some(b"stream".as_slice()) {
            return None;
        }

        // Locate endstream: skip any trailing whitespace after the body.
        let mut endstream_keyword_offset = range.end;
        while let Some(b) = pdf_data.get(endstream_keyword_offset) {
            if b.is_ascii_whitespace() {
                endstream_keyword_offset += 1;
            } else {
                break;
            }
        }
        let endstream_end = endstream_keyword_offset + b"endstream".len();
        if pdf_data.get(endstream_keyword_offset..endstream_end)
            != Some(b"endstream".as_slice())
        {
            return None;
        }

        let endstream_preceded_by_eol = endstream_keyword_offset
            .checked_sub(1)
            .and_then(|i| pdf_data.get(i))
            .is_some_and(|&b| b == b'\n' || b == b'\r');

        Some(StreamKeywordLayout {
            stream_keyword_offset,
            endstream_keyword_offset,
            stream_keyword_eol_canonical,
            endstream_preceded_by_eol,
        })
    }

    /// Return the decoded data of the stream.
    ///
    /// Note that the result of this method will not be cached, so calling it multiple
    /// times is expensive.
    pub fn decoded(&self) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        self.decoded_image(&ImageDecodeParams::default())
            .map(|r| r.data)
    }

    /// Return the decoded data of the stream, and return image metadata
    /// if available.
    pub fn decoded_image(
        &self,
        image_params: &ImageDecodeParams,
    ) -> Result<FilterResult<'a>, DecodeFailure> {
        let data = self.raw_data();
        let filters_and_params = self.filters_and_params();

        let mut current: Option<FilterResult<'a>> = None;

        for (filter, params) in filters_and_params
            .filters
            .iter()
            .zip(filters_and_params.params.iter())
        {
            let new = filter.apply(
                current.as_ref().map(|c| c.data.as_ref()).unwrap_or(&data),
                params,
                image_params,
            )?;
            current = Some(new);
        }

        Ok(current.unwrap_or(FilterResult {
            data,
            image_data: None,
        }))
    }
}

/// Physical layout of a stream's `stream`/`endstream` keyword pair.
///
/// Byte offsets are positions into the **original PDF source bytes**,
/// not into any decoded buffer. Produced by
/// [`Stream::keyword_layout`]. Requires the `inspect` feature.
#[cfg(feature = "inspect")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StreamKeywordLayout {
    /// Byte offset of the `s` in `stream`.
    pub stream_keyword_offset: usize,
    /// Byte offset of the `e` in `endstream`.
    pub endstream_keyword_offset: usize,
    /// `true` when the byte(s) after `stream` are LF or CRLF. CR alone
    /// is not permitted per ISO 32000-1 §7.3.8.1.
    pub stream_keyword_eol_canonical: bool,
    /// `true` when the byte immediately preceding `endstream` is LF or CR.
    pub endstream_preceded_by_eol: bool,
}

impl Debug for Stream<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "Stream (len: {:?})", self.data.len())
    }
}

impl Skippable for Stream<'_> {
    fn skip(_: &mut Reader<'_>, _: bool) -> Option<()> {
        // A stream can never appear in a dict/array, so it should never be skipped.
        warn!("attempted to skip a stream object");

        None
    }
}

impl<'a> Readable<'a> for Stream<'a> {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let dict = r.read_with_context::<Dict<'_>>(ctx)?;

        if dict.contains_key(F) {
            warn!("encountered stream referencing external file, which is unsupported");

            return None;
        }

        let offset = r.offset();
        parse_proper(r, &dict)
            .or_else(|| {
                warn!("failed to parse stream, trying to parse it manually");

                r.jump(offset);
                parse_fallback(r, &dict)
            })
            .error_none("was unable to manually parse the stream")
    }
}

#[derive(Debug, Copy, Clone)]
/// A failure that can occur during decoding a data stream.
pub enum DecodeFailure {
    /// An image stream failed to decode.
    ImageDecode,
    /// A data stream failed to decode.
    StreamDecode,
    /// A failure occurred while decrypting a file.
    Decryption,
    /// An unknown failure occurred.
    Unknown,
}

/// An image color space.
#[derive(Debug, Copy, Clone)]
pub enum ImageColorSpace {
    /// Grayscale color space.
    Gray,
    /// RGB color space.
    Rgb,
    /// CMYK color space.
    Cmyk,
    /// An unknown color space.
    Unknown(u8),
}

/// Additional data that is extracted from some image streams.
pub struct ImageData {
    /// An optional alpha channel of the image.
    pub alpha: Option<Vec<u8>>,
    /// The color space of the image.
    pub color_space: Option<ImageColorSpace>,
    /// The bits per component of the image.
    pub bits_per_component: u8,
    /// The width of the image.
    pub width: u32,
    /// The height of the image.
    pub height: u32,
}

/// The result of applying a filter.
pub struct FilterResult<'a> {
    /// The decoded data.
    pub data: Cow<'a, [u8]>,
    /// Additional data that is extracted from JPX image streams.
    pub image_data: Option<ImageData>,
}

impl FilterResult<'_> {
    pub(crate) fn from_data(data: Vec<u8>) -> Self {
        Self {
            data: Cow::Owned(data),
            image_data: None,
        }
    }
}

fn parse_proper<'a>(r: &mut Reader<'a>, dict: &Dict<'a>) -> Option<Stream<'a>> {
    let length = dict.get::<u32>(LENGTH)?;

    r.skip_white_spaces_and_comments();
    r.forward_tag(b"stream")?;
    r.forward_tag(b"\n")
        .or_else(|| r.forward_tag(b"\r\n"))
        .or_else(|| r.forward_tag(b"\r"))?;
    let data = r.read_bytes(length as usize)?;
    r.skip_white_spaces();
    r.forward_tag(b"endstream")?;

    Some(Stream::new(data, dict.clone()))
}

fn parse_fallback<'a>(r: &mut Reader<'a>, dict: &Dict<'a>) -> Option<Stream<'a>> {
    let stream_offset = find_needle(r.tail()?, b"stream")?;
    r.read_bytes(stream_offset)?;
    r.forward_tag(b"stream")?;

    r.forward_tag(b"\n")
        .or_else(|| r.forward_tag(b"\r\n"))
        // Technically not allowed, but no reason to not try it.
        .or_else(|| r.forward_tag(b"\r"))?;

    let tail = r.tail()?;
    let endstream_offset = find_needle(tail, b"endstream")?;
    let data_end = trim_trailing_ascii_whitespace(&tail[..endstream_offset]);
    let data = tail.get(..data_end)?;

    r.read_bytes(endstream_offset)?;
    r.skip_white_spaces();
    r.forward_tag(b"endstream")?;

    Some(Stream::new(data, dict.clone()))
}

fn trim_trailing_ascii_whitespace(data: &[u8]) -> usize {
    let mut end = data.len();

    while data
        .get(end.wrapping_sub(1))
        .copied()
        .is_some_and(is_white_space_character)
    {
        end -= 1;
    }

    end
}

impl<'a> TryFrom<Object<'a>> for Stream<'a> {
    type Error = ();

    fn try_from(value: Object<'a>) -> Result<Self, Self::Error> {
        match value {
            Object::Stream(s) => Ok(s),
            _ => Err(()),
        }
    }
}

impl<'a> ObjectLike<'a> for Stream<'a> {}
impl<'a> ObjectRefLike<'a> for Stream<'a> {
    fn cast_ref<'b>(obj: &'b Object<'a>) -> Option<&'b Self> {
        match obj {
            Object::Stream(stream) => Some(stream),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::object::Stream;
    use crate::reader::Reader;
    use crate::reader::{ReaderContext, ReaderExt};

    #[test]
    fn stream() {
        let data = b"<< /Length 10 >> stream\nabcdefghij\nendstream";
        let mut r = Reader::new(data);
        let stream = r
            .read_with_context::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();

        assert_eq!(stream.data, b"abcdefghij");
    }

    #[test]
    fn stream_fallback() {
        let data = b"<< /Length 999 >> stream\nabcdefghij\nendstream";
        let mut r = Reader::new(data);
        let stream = r
            .read_with_context::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();

        assert_eq!(stream.data, b"abcdefghij");
    }

    // --- PR #10: body range & keyword layout ------------------------------

    #[cfg(feature = "inspect")]
    mod pr10 {
        use crate::Pdf;
        use crate::object::Object;
        use alloc::format;
        use alloc::vec::Vec;

        /// Build a valid PDF with a stream at obj 3 0. `dict_extra` is
        /// extra dict entries (e.g. `" /Filter /FlateDecode"`). `eol`
        /// chooses the EOL after the `stream` keyword. `post_body` is
        /// inserted between the body and `endstream`.
        fn build_pdf_with_stream(
            body: &[u8],
            dict_extra: &str,
            eol: &[u8],
            post_body: &[u8],
        ) -> Vec<u8> {
            let mut pdf: Vec<u8> = Vec::new();
            pdf.extend_from_slice(b"%PDF-1.7\n");
            let off1 = pdf.len();
            pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
            let off2 = pdf.len();
            pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
            let off3 = pdf.len();
            pdf.extend_from_slice(
                format!("3 0 obj\n<< /Length {}{} >>\nstream", body.len(), dict_extra).as_bytes(),
            );
            pdf.extend_from_slice(eol);
            pdf.extend_from_slice(body);
            pdf.extend_from_slice(post_body);
            pdf.extend_from_slice(b"endstream\nendobj\n");

            let xref_pos = pdf.len();
            pdf.extend_from_slice(b"xref\n0 4\n");
            pdf.extend_from_slice(b"0000000000 65535 f \n");
            pdf.extend_from_slice(format!("{off1:010} 00000 n \n").as_bytes());
            pdf.extend_from_slice(format!("{off2:010} 00000 n \n").as_bytes());
            pdf.extend_from_slice(format!("{off3:010} 00000 n \n").as_bytes());
            pdf.extend_from_slice(b"trailer\n<< /Size 4 /Root 1 0 R >>\n");
            pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF").as_bytes());
            pdf
        }

        fn first_stream(pdf: &Pdf) -> Option<Stream<'_>> {
            for obj in pdf.objects() {
                if let Object::Stream(s) = obj {
                    return Some(s);
                }
            }
            None
        }

        use super::Stream;

        #[test]
        fn canonical_unfiltered_stream_body_range_and_keyword_layout() {
            let bytes = build_pdf_with_stream(b"hello", "", b"\n", b"\n");
            let pdf = Pdf::new(bytes).expect("pdf loads");
            let stream = first_stream(&pdf).expect("stream");
            let range = stream.body_range().expect("body range");
            assert_eq!(range.len(), 5);
            assert_eq!(stream.unfiltered_body_range(), Some(range.clone()));
            let layout = stream.keyword_layout().expect("keyword layout");
            assert!(layout.stream_keyword_eol_canonical);
            assert!(layout.endstream_preceded_by_eol);
            assert!(layout.stream_keyword_offset < range.start);
            assert!(layout.endstream_keyword_offset >= range.end);
        }

        #[test]
        fn cr_alone_after_stream_is_not_canonical() {
            let bytes = build_pdf_with_stream(b"hello", "", b"\r", b"\n");
            let pdf = Pdf::new(bytes).expect("pdf loads");
            let stream = first_stream(&pdf).expect("stream");
            let layout = stream.keyword_layout().expect("keyword layout");
            assert!(!layout.stream_keyword_eol_canonical);
        }

        #[test]
        fn crlf_after_stream_is_canonical() {
            let bytes = build_pdf_with_stream(b"hello", "", b"\r\n", b"\n");
            let pdf = Pdf::new(bytes).expect("pdf loads");
            let stream = first_stream(&pdf).expect("stream");
            let layout = stream.keyword_layout().expect("keyword layout");
            assert!(layout.stream_keyword_eol_canonical);
        }

        #[test]
        fn endstream_without_preceding_eol_flag_false() {
            // No EOL between body and endstream.
            let bytes = build_pdf_with_stream(b"hello", "", b"\n", b"");
            let pdf = Pdf::new(bytes).expect("pdf loads");
            let stream = first_stream(&pdf).expect("stream");
            let layout = stream.keyword_layout().expect("keyword layout");
            assert!(!layout.endstream_preceded_by_eol);
        }

        #[test]
        fn flate_filter_disables_unfiltered_body_range() {
            // Minimal valid zlib payload: two bytes of zero content ("\x78\x9c\x03\x00\x00\x00\x00\x01").
            let flate_body: &[u8] = b"\x78\x9c\x03\x00\x00\x00\x00\x01";
            let bytes = build_pdf_with_stream(flate_body, " /Filter /FlateDecode", b"\n", b"\n");
            let pdf = Pdf::new(bytes).expect("pdf loads");
            let stream = first_stream(&pdf).expect("stream");
            assert!(stream.body_range().is_some());
            assert_eq!(stream.unfiltered_body_range(), None);
        }

        #[test]
        fn identity_filter_is_treated_as_no_filter() {
            // `/Identity` is not a recognised reversing filter; filters() stays empty.
            let bytes = build_pdf_with_stream(b"hello", " /Filter /Identity", b"\n", b"\n");
            let pdf = Pdf::new(bytes).expect("pdf loads");
            let stream = first_stream(&pdf).expect("stream");
            let body = stream.body_range().expect("body range");
            assert_eq!(stream.unfiltered_body_range(), Some(body));
        }

        #[test]
        fn body_range_bytes_match_source() {
            let bytes = build_pdf_with_stream(b"hello world", "", b"\n", b"\n");
            let pdf = Pdf::new(bytes.clone()).expect("pdf loads");
            let stream = first_stream(&pdf).expect("stream");
            let range = stream.body_range().expect("body range");
            assert_eq!(&bytes[range], b"hello world");
        }

        #[test]
        fn flate_body_range_matches_source_length() {
            let flate_body: &[u8] = b"\x78\x9c\x03\x00\x00\x00\x00\x01";
            let bytes = build_pdf_with_stream(flate_body, " /Filter /FlateDecode", b"\n", b"\n");
            let pdf = Pdf::new(bytes.clone()).expect("pdf loads");
            let stream = first_stream(&pdf).expect("stream");
            let range = stream.body_range().expect("body range");
            assert_eq!(range.len(), flate_body.len());
            assert_eq!(&bytes[range], flate_body);
        }

        #[test]
        fn tier_b_flate_fixture_body_range() {
            // andler-optimal-lot-size has Flate-compressed streams.
            let bytes: &[u8] = include_bytes!(
                "../../../hayro-tests/pdfs/custom/andler-optimal-lot-size.pdf"
            );
            let pdf = Pdf::new(bytes.to_vec()).expect("fixture loads");
            let stream = first_stream(&pdf).expect("fixture has at least one stream");
            let range = stream.body_range().expect("body range");
            assert!(!range.is_empty());
            // Declared /Length must match the byte-range length for a
            // well-formed stream.
            let declared: Option<u32> = stream.dict().get(b"Length");
            if let Some(n) = declared {
                assert_eq!(range.len(), n as usize);
            }
        }
    }
}
