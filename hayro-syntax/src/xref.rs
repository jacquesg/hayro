//! Reading and querying the xref table of a PDF file.

use crate::crypto::{DecryptionError, DecryptionTarget, Decryptor, get};
use crate::data::Data;
use crate::metadata::Metadata;
use crate::object::Name;
use crate::object::ObjectIdentifier;
use crate::object::Stream;
use crate::object::dict::keys::{
    AUTHOR, CREATION_DATE, CREATOR, ENCRYPT, FIRST, ID, INDEX, INFO, KEYWORDS, MOD_DATE, N,
    OCPROPERTIES, PAGES, PREV, PRODUCER, ROOT, SIZE, SUBJECT, TITLE, TYPE, VERSION, W, XREF_STM,
};
use crate::object::dict::probe_dict;
use crate::object::indirect::IndirectObject;
use crate::object::{Array, MaybeRef};
use crate::object::{DateTime, Dict};
use crate::object::{Object, ObjectLike};
use crate::pdf::PdfVersion;
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt};
use crate::sync::{Arc, FxHashMap, RwLock, RwLockExt};
use crate::trivia::is_white_space_character;
use crate::util::findr_needle;
use crate::{PdfData, object};
use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::max;
use core::iter;
use core::ops::Deref;

pub(crate) const XREF_ENTRY_LEN: usize = 20;

#[derive(Debug, Copy, Clone)]
pub(crate) enum XRefError {
    Unknown,
    Encryption(DecryptionError),
}

/// Parse the "root" xref from the PDF.
pub(crate) fn root_xref(data: PdfData, password: &[u8]) -> Result<XRef, XRefError> {
    let mut xref_map = FxHashMap::default();
    let xref_pos = find_last_xref_pos(data.as_ref()).ok_or(XRefError::Unknown)?;
    let trailer =
        populate_xref_impl(data.as_ref(), xref_pos, &mut xref_map).ok_or(XRefError::Unknown)?;

    XRef::new(
        data.clone(),
        xref_map,
        XRefInput::TrailerDictData(trailer),
        false,
        password,
    )
}

/// Try to manually parse the PDF to build an xref table and trailer dictionary.
pub(crate) fn fallback(data: PdfData, password: &[u8]) -> Option<XRef> {
    warn!("xref table was invalid, trying to manually build xref table");
    let (xref_map, xref_input) = fallback_xref_map(&data, password);

    if let Some(xref_input) = xref_input {
        warn!("rebuild xref table with {} entries", xref_map.len());

        XRef::new(data.clone(), xref_map, xref_input, true, password).ok()
    } else {
        warn!("couldn't find trailer dictionary, failed to rebuild xref table");

        None
    }
}

fn fallback_xref_map<'a>(data: &'a PdfData, password: &[u8]) -> (XrefMap, Option<XRefInput<'a>>) {
    fallback_xref_map_inner(data, ReaderContext::dummy(), true, password)
}

fn fallback_xref_map_inner<'a>(
    data: &'a PdfData,
    mut dummy_ctx: ReaderContext<'a>,
    recurse: bool,
    password: &[u8],
) -> (XrefMap, Option<XRefInput<'a>>) {
    let mut xref_map = FxHashMap::default();
    let mut trailer_dicts = vec![];
    let mut root_ref = None;

    let mut r = Reader::new(data.as_ref());

    let mut last_obj_num = None;

    loop {
        let cur_pos = r.offset();

        let mut old_r = r.clone();

        // First try to check if we have an object identifier.
        if r.peek_byte().is_some_and(|b: u8| b.is_ascii_digit()) {
            if let Some(obj_id) = r.read::<ObjectIdentifier>(&dummy_ctx) {
                let mut cloned = r.clone();
                // Check that the object following it is actually valid before inserting it.
                cloned.skip_white_spaces_and_comments();
                if cloned.skip::<Object<'_>>(false).is_some() {
                    xref_map.insert(obj_id, EntryType::Normal { offset: cur_pos });
                    last_obj_num = Some(obj_id);
                    dummy_ctx.set_obj_number(obj_id);
                }
            } else {
                // There must be a white space before the next object number.
                r.forward_while(|b| !is_white_space_character(b));
            }
        } else {
            // Then, try to check whether we have a dictionary, in particular a trailer
            // dictionary.
            let mut probe_reader = r.clone();
            if r.peek_bytes(2).is_some_and(|b| b == b"<<")
                && let Some(probe) =
                    { probe_dict(&mut probe_reader, &dummy_ctx, Some(b"<<"), b">>") }
            {
                r = probe_reader;
                if probe.has_root || probe.has_type {
                    let mut dict_reader = Reader::new(probe.data);
                    if let Some(dict) = dict_reader.read_with_context::<Dict<'_>>(&dummy_ctx) {
                        if probe.has_root && dict.contains_key(ROOT) {
                            trailer_dicts.push(dict.clone());
                        }

                        if dict
                            .get::<Name<'_>>(TYPE)
                            .is_some_and(|n| n.as_str() == "Catalog")
                        {
                            root_ref = last_obj_num;
                        }

                        if let Some(stream) = old_r.read::<Stream<'_>>(&dummy_ctx)
                            && dict.get::<Name<'_>>(TYPE).as_deref() == Some(b"ObjStm")
                            && let Some(data) = stream.decoded().ok()
                            && let Some(last_obj_num) = last_obj_num
                            && let Some(obj_stream) = ObjectStream::new(stream, &data, &dummy_ctx)
                        {
                            for (idx, (obj_num, _)) in obj_stream.offsets.iter().enumerate() {
                                let id = ObjectIdentifier::new(*obj_num as i32, 0);
                                // If we already found an entry for that object number that was not
                                // inside an object stream. Somewhat arbitrary and maybe
                                // we can do better, but that seems to work for the current
                                // set of tests.
                                if xref_map
                                    .get(&id)
                                    .is_none_or(|e| !matches!(e, &EntryType::Normal { .. }))
                                {
                                    xref_map.insert(
                                        id,
                                        EntryType::Compressed {
                                            obj_stream: last_obj_num.obj_number,
                                            index: idx as u32,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            } else {
                // We can skip everything until the next white space character,
                // as there cannot possibly be any new dictionary/object identifier
                // until then.
                let old_pos = r.offset;
                r.forward_while(|b| !is_white_space_character(b));
                if r.offset == old_pos {
                    r.read_byte();
                }
            }
        }

        if r.at_end() {
            break;
        }
    }

    // Try to choose the right trailer dict by doing basic validation.
    let mut trailer_dict = None;

    for dict in trailer_dicts {
        if let Some(root_id) = dict.get_raw::<Dict<'_>>(ROOT) {
            let check = |dict: &Dict<'_>| -> bool { dict.contains_key(PAGES) };

            match root_id {
                MaybeRef::Ref(r) => match xref_map.get(&r.into()) {
                    Some(EntryType::Normal { offset }) => {
                        let mut reader = Reader::new(&data.as_ref()[*offset..]);

                        if let Some(obj) =
                            reader.read_with_context::<IndirectObject<Dict<'_>>>(&dummy_ctx)
                            && {
                                let obj = obj.get();
                                check(&obj)
                            }
                        {
                            trailer_dict = Some(dict);
                        }
                    }
                    Some(EntryType::Compressed { obj_stream, index }) => {
                        if let Some(EntryType::Normal { offset }) =
                            xref_map.get(&ObjectIdentifier::new(*obj_stream, 0))
                        {
                            let mut reader = Reader::new(&data.as_ref()[*offset..]);

                            if let Some(stream) =
                                reader.read_with_context::<IndirectObject<Stream<'_>>>(&dummy_ctx)
                                && {
                                    let stream = stream.get();
                                    if let Some(data) = stream.decoded().ok()
                                        && let Some(object_stream) =
                                            ObjectStream::new(stream, &data, &dummy_ctx)
                                        && let Some(obj) = object_stream.get::<Dict<'_>>(*index)
                                    {
                                        check(&obj)
                                    } else {
                                        false
                                    }
                                }
                            {
                                trailer_dict = Some(dict);
                            }
                        }
                    }
                    _ => {}
                },
                MaybeRef::NotRef(d) => {
                    if check(&d) {
                        trailer_dict = Some(dict);
                    }
                }
            }
        }
    }

    let has_encryption = trailer_dict
        .as_ref()
        .is_some_and(|t| t.contains_key(ENCRYPT));

    if has_encryption && recurse {
        // The problem is that in this case, we have used a dummy reader context which does not have
        // a decryptor. Therefore, we were unable to decrypt any of the object streams and missed
        // all objects that are inside of such a stream. Therefore, we need to redo the process
        // using a `ReaderContext` that does have the ability to decrypt.
        if let Ok(xref) = XRef::new(
            data.clone(),
            xref_map.clone(),
            XRefInput::TrailerDictData(trailer_dict.as_ref().map(|d| d.data()).unwrap()),
            true,
            password,
        ) {
            let ctx = ReaderContext::new(&xref, false);
            let (patched_map, _) = fallback_xref_map_inner(data, ctx, false, password);
            xref_map = patched_map;
        }
    }

    if let Some(trailer_dict_data) = trailer_dict.map(|d| d.data()) {
        (
            xref_map,
            Some(XRefInput::TrailerDictData(trailer_dict_data)),
        )
    } else if let Some(root_ref) = root_ref {
        (xref_map, Some(XRefInput::RootRef(root_ref)))
    } else {
        (xref_map, None)
    }
}

const DUMMY_XREF: XRef = XRef(Inner::Dummy);

/// An xref table.
#[derive(Debug, Clone)]
pub struct XRef(Inner);

impl XRef {
    fn new(
        data: PdfData,
        xref_map: XrefMap,
        input: XRefInput<'_>,
        repaired: bool,
        password: &[u8],
    ) -> Result<Self, XRefError> {
        // This is a bit hacky, but the problem is we can't read the resolved trailer dictionary
        // before we actually created the xref struct. So we first create it using dummy data
        // and then populate the data.
        let trailer_data = TrailerData::dummy();

        let trailer_dict_bytes = match input {
            XRefInput::TrailerDictData(bytes) => Some(Arc::from(bytes)),
            XRefInput::RootRef(_) => None,
        };

        // Streaming sources bound each object's on-demand read at the next
        // object's offset; precompute the sorted offset index here (resident
        // sources read the whole buffer and leave it empty).
        let sorted_offsets = if data.is_streamed() {
            let mut offsets: Vec<u64> = xref_map
                .values()
                .filter_map(|entry| match entry {
                    EntryType::Normal { offset } => Some(*offset as u64),
                    _ => None,
                })
                .collect();
            offsets.sort_unstable();
            offsets.dedup();
            offsets.push(data.len());
            offsets
        } else {
            Vec::new()
        };

        let mut xref = Self(Inner::Some(Arc::new(SomeRepr {
            data: Arc::new(Data::new(data)),
            map: Arc::new(RwLock::new(MapRepr { xref_map, repaired })),
            decryptor: Arc::new(Decryptor::None),
            has_ocgs: false,
            metadata: Arc::new(Metadata::default()),
            trailer_data,
            trailer_dict_bytes,
            password: password.to_vec(),
            sorted_offsets,
        })));

        // We read the trailer twice, once to determine the encryption used and then a second
        // time to resolve the catalog dictionary, etc. This allows us to support catalog dictionaries
        // that are stored in an encrypted object stream.

        let decryptor = {
            match input {
                XRefInput::TrailerDictData(trailer_dict_data) => {
                    let mut r = Reader::new(trailer_dict_data);

                    let trailer_dict = r
                        .read_with_context::<Dict<'_>>(&ReaderContext::new(&xref, false))
                        .ok_or(XRefError::Unknown)?;

                    get_decryptor(&trailer_dict, password)?
                }
                XRefInput::RootRef(_) => Decryptor::None,
            }
        };

        match &mut xref.0 {
            Inner::Dummy => unreachable!(),
            Inner::Some(r) => {
                let mutable = Arc::make_mut(r);
                mutable.decryptor = Arc::new(decryptor.clone());
            }
        }

        let (trailer_data, has_ocgs, metadata) = match input {
            XRefInput::TrailerDictData(trailer_dict_data) => {
                let mut r = Reader::new(trailer_dict_data);

                let trailer_dict = r
                    .read_with_context::<Dict<'_>>(&ReaderContext::new(&xref, false))
                    .ok_or(XRefError::Unknown)?;

                let root_ref = trailer_dict.get_ref(ROOT).ok_or(XRefError::Unknown)?;
                let root = trailer_dict
                    .get::<Dict<'_>>(ROOT)
                    .ok_or(XRefError::Unknown)?;
                let metadata = trailer_dict
                    .get::<Dict<'_>>(INFO)
                    .map(|d| parse_metadata(&d))
                    .unwrap_or_default();
                let pages_ref = root.get_ref(PAGES).ok_or(XRefError::Unknown)?;
                let has_ocgs = root.get::<Dict<'_>>(OCPROPERTIES).is_some();
                let version = root
                    .get::<Name<'_>>(VERSION)
                    .and_then(|v| PdfVersion::from_bytes(v.deref()));

                let td = TrailerData {
                    pages_ref: pages_ref.into(),
                    root_ref: root_ref.into(),
                    version,
                };

                (td, has_ocgs, metadata)
            }
            XRefInput::RootRef(root_ref) => {
                let root = xref.get::<Dict<'_>>(root_ref).ok_or(XRefError::Unknown)?;
                let pages_ref = root.get_ref(PAGES).ok_or(XRefError::Unknown)?;

                let td = TrailerData {
                    pages_ref: pages_ref.into(),
                    root_ref,
                    version: None,
                };

                (td, false, Metadata::default())
            }
        };

        match &mut xref.0 {
            Inner::Dummy => unreachable!(),
            Inner::Some(r) => {
                let mutable = Arc::make_mut(r);
                mutable.trailer_data = trailer_data;
                mutable.decryptor = Arc::new(decryptor);
                mutable.has_ocgs = has_ocgs;
                mutable.metadata = Arc::new(metadata);
            }
        }

        Ok(xref)
    }

    fn is_repaired(&self) -> bool {
        match &self.0 {
            Inner::Dummy => false,
            Inner::Some(r) => {
                let locked = r.map.get();
                locked.repaired
            }
        }
    }

    pub(crate) fn dummy() -> &'static Self {
        &DUMMY_XREF
    }

    pub(crate) fn len(&self) -> usize {
        match &self.0 {
            Inner::Dummy => 0,
            Inner::Some(r) => r
                .map
                .get()
                .xref_map
                .values()
                .filter(|e| !matches!(e, EntryType::Free { .. }))
                .count(),
        }
    }

    /// Resolve the cross-reference entry for the given object identifier.
    ///
    /// Returns `None` for a dummy xref or when `id` is not present in any
    /// xref section. The returned [`EntryType`] discriminates between
    /// [`Normal`](EntryType::Normal) (offset in file), [`Compressed`](
    /// EntryType::Compressed) (inside an object stream), and
    /// [`Free`](EntryType::Free) (on the free list).
    pub fn entry(&self, id: ObjectIdentifier) -> Option<EntryType> {
        match &self.0 {
            Inner::Dummy => None,
            Inner::Some(r) => r.map.get().xref_map.get(&id).copied(),
        }
    }

    /// Iterate over every entry in the cross-reference table, including
    /// free entries.
    ///
    /// Ordering is deterministic but unspecified — do not rely on
    /// iteration order matching file order.
    pub fn entries(&self) -> Vec<(ObjectIdentifier, EntryType)> {
        match &self.0 {
            Inner::Dummy => Vec::new(),
            Inner::Some(r) => r
                .map
                .get()
                .xref_map
                .iter()
                .map(|(id, e)| (*id, *e))
                .collect(),
        }
    }

    /// Return the trailer dictionary of the first-page xref section in
    /// a linearized document.
    ///
    /// Returns `None` for non-linearized documents, for dummy xrefs, or
    /// when the first-page trailer cannot be located. The first-page
    /// trailer is the dict immediately preceding the first `%%EOF`
    /// marker in the file (ISO 32000-1 Annex F.4.5).
    ///
    /// Requires the `inspect` feature because it uses `FileLayout`
    /// EOF offsets.
    #[cfg(feature = "inspect")]
    pub fn first_page_trailer(&self) -> Option<Dict<'_>> {
        let repr = match &self.0 {
            Inner::Dummy => return None,
            Inner::Some(r) => r,
        };
        let data = repr.data.get().as_ref();

        let layout = crate::layout::FileLayout::compute(data);
        let first_eof = *layout.eof_offsets.first()?;

        // Within [0, first_eof), find the last `trailer` keyword.
        let scan_area = data.get(..first_eof)?;
        let trailer_keyword = b"trailer";
        let trailer_rel = rfind_subslice(scan_area, trailer_keyword)?;
        let dict_start = trailer_rel + trailer_keyword.len();
        let mut reader = Reader::new(data);
        reader.jump(dict_start);
        reader.skip_white_spaces_and_comments();
        reader.read_with_context::<Dict<'_>>(&ReaderContext::new(self, false))
    }

    /// Return the trailer dictionary pinned by `startxref`.
    ///
    /// The result is the trailer of the terminal xref section — the
    /// section reached by starting at the file's final `startxref`
    /// offset and walking `/Prev` links until no more remain. For a
    /// single-section document this is the same dict as
    /// [`XRef::trailer`]. For a document with multiple sections —
    /// most notably a linearised document where `startxref` points at
    /// the first-page xref and `/Prev` reaches the main xref at the
    /// tail — this returns the dict at the end of the `/Prev` chain,
    /// which differs from `trailer()`.
    ///
    /// Useful for diagnostics that compare first-page versus
    /// tail-of-file trailer state (e.g. matching `/ID` arrays across
    /// both sections).
    ///
    /// Returns `None` for dummy xrefs, for xrefs reconstructed from a
    /// fallback root reference, or when the chain cannot be walked to
    /// its terminus.
    ///
    /// # Performance
    ///
    /// Walks the `/Prev` chain from source bytes on each call; one
    /// call is O(chain length). Consumers reading several fields from
    /// the same trailer should bind the result once.
    ///
    /// Requires the `inspect` feature.
    #[cfg(feature = "inspect")]
    pub fn latest_trailer(&self) -> Option<Dict<'_>> {
        let repr = match &self.0 {
            Inner::Dummy => return None,
            Inner::Some(r) => r,
        };
        let data = repr.data.get().as_ref();
        let start_pos = find_last_xref_pos(data)?;
        let mut sections: Vec<XRefSection> = Vec::new();
        let mut visited: BTreeSet<usize> = BTreeSet::new();
        collect_sections(data, start_pos, &mut sections, &mut visited);
        let terminal = sections.last()?;
        let ctx = ReaderContext::new(self, false);
        match terminal.kind {
            XRefKind::Table => {
                let mut reader = Reader::new(data);
                reader.jump(terminal.keyword_offset);
                read_xref_table_trailer(&mut reader, &ctx)
            }
            XRefKind::Stream => {
                let mut reader = Reader::new(data);
                reader.jump(terminal.keyword_offset);
                let stream = reader
                    .read_with_context::<IndirectObject<Stream<'_>>>(&ctx)?
                    .get();
                // The stream dict is the trailer for an xref stream
                // (§7.5.8.1). Clone to detach from the local IndirectObject.
                Some(stream.dict().clone())
            }
        }
    }

    /// Return all cross-reference sections, ordered most-recent-first.
    ///
    /// The sections are discovered by walking the file's `startxref`
    /// and following each section's `/Prev` entry. Ordering follows the
    /// walk, so the section reached via the final `startxref` appears
    /// first and each predecessor follows.
    ///
    /// # Performance
    ///
    /// Walks the chain from the retained PDF data on each call; no
    /// section-tracking state is retained at parse time. One call is
    /// O(chain length) and allocates one `Vec`. Consumers should bind
    /// the result once.
    ///
    /// Requires the `inspect` feature.
    #[cfg(feature = "inspect")]
    pub fn sections(&self) -> Vec<XRefSection> {
        match &self.0 {
            Inner::Dummy => Vec::new(),
            Inner::Some(r) => {
                let data = r.data.get().as_ref();
                let Some(start_pos) = find_last_xref_pos(data) else {
                    return Vec::new();
                };
                let mut out: Vec<XRefSection> = Vec::new();
                let mut visited: BTreeSet<usize> = BTreeSet::new();
                collect_sections(data, start_pos, &mut out, &mut visited);
                out
            }
        }
    }

    /// Resolve layout information for an indirect object.
    ///
    /// Returns `None` when `id` is absent from the xref table. For every
    /// present entry — `Normal`, `Compressed`, or `Free` — a
    /// [`LayoutKind`](crate::layout::LayoutKind) variant names the case
    /// explicitly; callers do not have to hop through the hosting
    /// object stream themselves.
    ///
    /// Malformed cases (e.g. a compressed entry whose host is itself
    /// compressed or free — illegal per ISO 32000-1 §7.5.7) also return
    /// `None`; hayro does not attempt recovery.
    ///
    /// Requires the `inspect` feature.
    #[cfg(feature = "inspect")]
    pub fn indirect_layout(
        &self,
        id: ObjectIdentifier,
    ) -> Option<crate::layout::LayoutKind> {
        use crate::layout::{LayoutKind, scan_indirect_layout};

        let repr = match &self.0 {
            Inner::Dummy => return None,
            Inner::Some(r) => r,
        };
        let entry = repr.map.get().xref_map.get(&id).copied()?;
        let data = repr.data.get().as_ref();

        match entry {
            EntryType::Free { .. } => Some(LayoutKind::Free),
            EntryType::Normal { offset } => {
                let layout = scan_indirect_layout(data, offset)?;
                Some(LayoutKind::Direct(layout))
            }
            EntryType::Compressed { obj_stream, index } => {
                let host = ObjectIdentifier::new(obj_stream, 0);
                let host_entry = repr.map.get().xref_map.get(&host).copied()?;
                let host_offset = match host_entry {
                    EntryType::Normal { offset } => offset,
                    // Host of a compressed entry must itself be direct.
                    _ => return None,
                };
                let host_layout = scan_indirect_layout(data, host_offset)?;
                Some(LayoutKind::Compressed {
                    host,
                    host_layout,
                    index,
                })
            }
        }
    }

    /// Return the logical size of the cross-reference table.
    ///
    /// This is the trailer's `/Size` value when present, falling back to
    /// the highest observed object number plus one.
    pub fn size(&self) -> i32 {
        let from_trailer: Option<i32> =
            self.trailer().and_then(|t| t.get::<i32>(SIZE));
        let from_map: i32 = match &self.0 {
            Inner::Dummy => 0,
            Inner::Some(r) => r
                .map
                .get()
                .xref_map
                .keys()
                .map(|k| k.obj_number.saturating_add(1))
                .max()
                .unwrap_or(0),
        };
        max(from_trailer.unwrap_or(0), from_map)
    }

    /// Return the raw PDF source bytes backing this xref, if any.
    #[cfg(feature = "inspect")]
    pub(crate) fn data_bytes(&self) -> Option<&[u8]> {
        match &self.0 {
            Inner::Dummy => None,
            Inner::Some(r) => Some(r.data.get().as_ref()),
        }
    }

    pub(crate) fn trailer_data(&self) -> &TrailerData {
        match &self.0 {
            Inner::Dummy => unreachable!(),
            Inner::Some(r) => &r.trailer_data,
        }
    }

    pub(crate) fn metadata(&self) -> &Metadata {
        match &self.0 {
            Inner::Dummy => unreachable!(),
            Inner::Some(r) => &r.metadata,
        }
    }

    /// Return the object ID of the root dictionary.
    pub fn root_id(&self) -> ObjectIdentifier {
        self.trailer_data().root_ref
    }

    /// Return the document's trailer dictionary.
    ///
    /// For a conventional xref table this is the dict following the last
    /// `trailer` keyword; for an xref stream it is the stream's own dict.
    /// Returns `None` for a dummy xref, or when the original trailer could
    /// not be recovered and the xref was reconstructed from a fallback
    /// root reference.
    ///
    /// For a document with incremental updates, this is the trailer
    /// reached via the final `startxref`, as specified by
    /// ISO 32000-1 §7.5.6.
    ///
    /// # Performance
    ///
    /// This re-parses the trailer dict from a cached byte slice on every
    /// call. The parse is cheap (proportional to the dict size, no xref
    /// traversal) but not free; consumers making many reads from the same
    /// trailer should bind the result once rather than re-calling the
    /// method in a loop:
    ///
    /// ```ignore
    /// let trailer = xref.trailer()?;
    /// let id = trailer.get::<Array>(b"ID");
    /// let size: Option<i32> = trailer.get(b"Size");
    /// ```
    pub fn trailer(&self) -> Option<Dict<'_>> {
        match &self.0 {
            Inner::Dummy => None,
            Inner::Some(r) => {
                let bytes = r.trailer_dict_bytes.as_deref()?;
                let mut reader = Reader::new(bytes);
                reader.read_with_context::<Dict<'_>>(&ReaderContext::new(self, false))
            }
        }
    }

    /// Return the document's encryption dictionary, if any.
    ///
    /// Resolves the trailer's `/Encrypt` entry, following an indirect
    /// reference where needed. Returns `None` for unencrypted documents,
    /// for dummy xrefs, and for xrefs reconstructed from a fallback root
    /// reference (where the original trailer is unavailable).
    ///
    /// The accessor reports the presence of `/Encrypt` in the trailer and
    /// is independent of whether decryption succeeded: a document loaded
    /// via [`Pdf::new_with_password`] will still report an encryption
    /// dict here.
    ///
    /// # Performance
    ///
    /// Re-parses the trailer on each call. See [`XRef::trailer`] for the
    /// same binding guidance.
    ///
    /// [`Pdf::new_with_password`]: crate::Pdf::new_with_password
    pub fn encryption_dict(&self) -> Option<Dict<'_>> {
        self.trailer()?.get::<Dict<'_>>(ENCRYPT)
    }

    /// Whether the document is encrypted.
    ///
    /// Equivalent to `self.encryption_dict().is_some()`.
    pub fn is_encrypted(&self) -> bool {
        self.encryption_dict().is_some()
    }

    /// Whether the PDF has optional content groups.
    pub fn has_optional_content_groups(&self) -> bool {
        match &self.0 {
            Inner::Dummy => false,
            Inner::Some(r) => r.has_ocgs,
        }
    }

    pub(crate) fn objects(&self) -> impl IntoIterator<Item = Object<'_>> + '_ {
        match &self.0 {
            Inner::Dummy => unimplemented!(),
            Inner::Some(r) => {
                let locked = r.map.get();
                let mut elements = locked
                    .xref_map
                    .iter()
                    .filter_map(|(id, e)| {
                        let offset = match e {
                            EntryType::Normal { offset } => (*offset, 0),
                            EntryType::Compressed { obj_stream, index } => {
                                if let Some(EntryType::Normal { offset }) =
                                    locked.xref_map.get(&ObjectIdentifier::new(*obj_stream, 0))
                                {
                                    (*offset, *index)
                                } else {
                                    (usize::MAX, 0)
                                }
                            }
                            // Free entries have no object body to yield.
                            EntryType::Free { .. } => return None,
                        };

                        Some((*id, offset))
                    })
                    .collect::<Vec<_>>();

                // Try to yield in the order the objects appeared in the
                // PDF.
                elements.sort_by(|e1, e2| e1.1.cmp(&e2.1));

                let mut iter = elements.into_iter();

                iter::from_fn(move || {
                    for next in iter.by_ref() {
                        if let Some(obj) = self.get_with(next.0, &ReaderContext::new(self, false)) {
                            return Some(obj);
                        } else {
                            // Skip invalid objects.
                            continue;
                        }
                    }

                    None
                })
            }
        }
    }

    pub(crate) fn repair(&self) {
        let Inner::Some(r) = &self.0 else {
            unreachable!();
        };

        let mut locked = r.map.try_put().unwrap();
        assert!(!locked.repaired);

        let (xref_map, _) = fallback_xref_map(r.data.get(), &r.password);
        locked.xref_map = xref_map;
        locked.repaired = true;
    }

    #[inline]
    pub(crate) fn needs_decryption(&self, ctx: &ReaderContext<'_>) -> bool {
        match &self.0 {
            Inner::Dummy => false,
            Inner::Some(r) => {
                if matches!(r.decryptor.as_ref(), Decryptor::None) {
                    false
                } else {
                    !ctx.in_content_stream() && !ctx.in_object_stream()
                }
            }
        }
    }

    #[inline]
    pub(crate) fn decrypt(
        &self,
        id: ObjectIdentifier,
        data: &[u8],
        target: DecryptionTarget,
    ) -> Option<Vec<u8>> {
        match &self.0 {
            Inner::Dummy => Some(data.to_vec()),
            Inner::Some(r) => r.decryptor.decrypt(id, data, target),
        }
    }

    /// Return the object with the given identifier.
    #[allow(private_bounds)]
    pub fn get<'a, T>(&'a self, id: ObjectIdentifier) -> Option<T>
    where
        T: ObjectLike<'a>,
    {
        let ctx = ReaderContext::new(self, false);
        self.get_with(id, &ctx)
    }

    /// Return the object with the given identifier.
    #[allow(private_bounds)]
    pub(crate) fn get_with<'a, T>(
        &'a self,
        id: ObjectIdentifier,
        ctx: &ReaderContext<'a>,
    ) -> Option<T>
    where
        T: ObjectLike<'a>,
    {
        let Inner::Some(repr) = &self.0 else {
            return None;
        };

        let entry = {
            let locked = repr.map.try_get().unwrap();
            // An indirect reference to an undefined object shall not be considered an error by a PDF processor; it
            // shall be treated as a reference to the null object.
            match locked.xref_map.get(&id) {
                Some(entry) => *entry,
                None => return None,
            }
        };

        let mut ctx = ctx.clone();
        ctx.set_obj_number(id);
        ctx.set_in_content_stream(false);

        // Streaming source: read only the bytes each object needs, rather
        // than borrowing the whole (non-resident) buffer. The resident path
        // below is byte-for-byte unchanged.
        if repr.data.get().is_streamed() {
            return match entry {
                EntryType::Free { .. } => None,
                EntryType::Normal { offset } => {
                    ctx.set_in_object_stream(false);
                    let end_bound = repr.next_object_bound(offset);
                    if let Some(window) = repr.data.object_window(offset as u64, end_bound) {
                        let mut r = Reader::new(window);
                        if let Some(object) = r.read_with_context::<IndirectObject<T>>(&ctx) {
                            if object.id() == &id {
                                return Some(object.get());
                            }
                        } else if r
                            .skip_not_in_content_stream::<IndirectObject<Object<'_>>>()
                            .is_some()
                        {
                            // Valid object, wrong type - a clean miss.
                            return None;
                        }
                    }

                    // The object window did not parse; fall back to a
                    // whole-file repair (which materialises) once.
                    if self.is_repaired() {
                        error!(
                            "attempt was made at repairing xref, but object {id:?} still couldn't be read"
                        );
                        None
                    } else {
                        warn!("broken xref, attempting to repair");
                        self.repair();
                        self.get_with::<T>(id, &ctx)
                    }
                }
                EntryType::Compressed { obj_stream, index } => {
                    let obj_stream_id = ObjectIdentifier::new(obj_stream, 0);
                    if obj_stream_id == id {
                        warn!("cycle detected in object stream");
                        return None;
                    }
                    let stream = self.get_with::<Stream<'_>>(obj_stream_id, &ctx)?;
                    let data = repr.data.get_with(obj_stream_id, &ctx)?;
                    let object_stream = ObjectStream::new(stream, data, &ctx)?;
                    object_stream.get(index)
                }
            };
        }

        let mut r = Reader::new(repr.data.get().as_ref());

        match entry {
            EntryType::Free { .. } => None,
            EntryType::Normal { offset } => {
                ctx.set_in_object_stream(false);
                r.jump(offset);

                if let Some(object) = r.read_with_context::<IndirectObject<T>>(&ctx) {
                    if object.id() == &id {
                        return Some(object.get());
                    }
                } else {
                    // There is a valid object at the offset, it's just not of the type the caller
                    // expected, which is fine.
                    if r.skip_not_in_content_stream::<IndirectObject<Object<'_>>>()
                        .is_some()
                    {
                        return None;
                    }
                };

                // The xref table is broken, try to repair if not already repaired.
                if self.is_repaired() {
                    error!(
                        "attempt was made at repairing xref, but object {id:?} still couldn't be read"
                    );

                    None
                } else {
                    warn!("broken xref, attempting to repair");

                    self.repair();

                    // Now try reading again.
                    self.get_with::<T>(id, &ctx)
                }
            }
            EntryType::Compressed { obj_stream, index } => {
                // Generation number is implicitly 0.
                let obj_stream_id = ObjectIdentifier::new(obj_stream, 0);

                if obj_stream_id == id {
                    warn!("cycle detected in object stream");

                    return None;
                }

                let stream = self.get_with::<Stream<'_>>(obj_stream_id, &ctx)?;
                let data = repr.data.get_with(obj_stream_id, &ctx)?;
                let object_stream = ObjectStream::new(stream, data, &ctx)?;
                object_stream.get(index)
            }
        }
    }
}

/// An input that is passed to the xref constructor so that we can fully resolve
/// the PDF.
#[derive(Debug, Copy, Clone)]
pub(crate) enum XRefInput<'a> {
    /// This option is going to be uesd in 99.999% of the case. It contains the
    /// raw data of the trailer dictionary which is then going to be processed.
    TrailerDictData(&'a [u8]),
    /// In case the trailer dictionary could not be read (for example because
    /// it is cut-off), we just pass the object ID of the root dictionary
    /// in case we have found one, and try our best to build the PDF just
    /// with the information we have there.
    ///
    /// Note that this won't work if the document is encrypted, as we
    /// can't access the crypto dictionary.
    RootRef(ObjectIdentifier),
}

/// Return the byte offset of the last occurrence of `needle` in
/// `haystack`, or `None` if not present.
#[cfg(feature = "inspect")]
fn rfind_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .rev()
        .find(|&i| &haystack[i..i + needle.len()] == needle)
}

pub(crate) fn find_last_xref_pos(data: &[u8]) -> Option<usize> {
    let needle = b"startxref";
    let pos = findr_needle(data, needle)?;
    let mut finder = Reader::new(data);
    finder.jump(pos);
    finder.forward_tag(needle)?;
    finder.skip_white_spaces_and_comments();
    finder.read_without_context::<i32>()?.try_into().ok()
}

/// Streaming variant of [`find_last_xref_pos`]: locate `startxref` by reading
/// a growing tail window, instead of scanning the whole (non-resident) buffer.
fn find_last_xref_pos_streamed(data: &PdfData, file_len: u64) -> Option<usize> {
    // `startxref` lives in the final bytes of the file; a 1 MiB ceiling is far
    // more than any conformant trailer needs.
    const MAX_TAIL: u64 = 1 << 20;
    let mut win = core::cmp::min(1024, file_len.max(1));
    loop {
        let start = file_len.saturating_sub(win);
        let tail = data.read_range(start, win as usize);
        if let Some(rel) = findr_needle(&tail, b"startxref") {
            let mut finder = Reader::new(&tail);
            finder.jump(rel);
            finder.forward_tag(b"startxref")?;
            finder.skip_white_spaces_and_comments();
            return finder.read_without_context::<i32>()?.try_into().ok();
        }
        if start == 0 || win >= MAX_TAIL {
            return None;
        }
        win = core::cmp::min(win * 2, file_len);
    }
}

/// Streaming variant of [`root_xref`]: build the cross-reference table from
/// bounded positioned reads of `data`, without materialising the whole file.
///
/// Only a single, non-hybrid xref section is handled here. Multi-section
/// (`/Prev`) or hybrid (`/XRefStm`) files return [`XRefError::Unknown`] so the
/// caller can fall back to a resident parse; object bodies are then still read
/// on demand by [`XRef::get_with`]'s streaming path.
pub(crate) fn root_xref_streamed(
    data: PdfData,
    password: &[u8],
    file_len: u64,
) -> Result<XRef, XRefError> {
    let xref_pos = find_last_xref_pos_streamed(&data, file_len).ok_or(XRefError::Unknown)?;
    let win_len = usize::try_from(file_len.saturating_sub(xref_pos as u64))
        .map_err(|_| XRefError::Unknown)?;
    // The window starts at the xref section, so `populate_xref_impl` reads it
    // at relative offset 0. Object offsets it stores remain absolute file
    // offsets (they are data values, not reader positions).
    let window = data.read_range(xref_pos as u64, win_len);

    let mut xref_map = FxHashMap::default();
    let trailer = populate_xref_impl(&window, 0, &mut xref_map).ok_or(XRefError::Unknown)?;

    // `/Prev` already aborted `populate_xref_impl` (its absolute offset falls
    // outside the tail window); a `/XRefStm` would be silently skipped. Detect
    // either in the trailer and defer to the resident fallback rather than
    // risk an incomplete map.
    if findr_needle(trailer, b"/Prev").is_some() || findr_needle(trailer, b"/XRefStm").is_some() {
        return Err(XRefError::Unknown);
    }

    XRef::new(
        data.clone(),
        xref_map,
        XRefInput::TrailerDictData(trailer),
        false,
        password,
    )
}

/// The type of a cross-reference table entry.
///
/// ISO 32000-1 §7.5.4 and §7.5.8 describe the three entry types.
///
/// `Free` entries form a linked list (`next_free`, `generation`);
/// diagnostics and repair tools need to walk it. `Compressed` entries
/// point into an object stream per §7.5.7.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[non_exhaustive]
pub enum EntryType {
    /// In-use object at the given byte offset into the original data.
    Normal {
        /// File byte offset of the `N G obj` header.
        offset: usize,
    },
    /// In-use object stored inside an object stream (§7.5.7).
    Compressed {
        /// Object number of the hosting object stream. The generation
        /// number of a compressed object is always 0 per spec.
        obj_stream: i32,
        /// Zero-based index of the object within the stream.
        index: u32,
    },
    /// A free entry on the free-list linked list (§7.5.4).
    Free {
        /// Object number of the next free entry. `0` terminates the list.
        next_free: i32,
        /// The generation number the next re-use of this slot must have.
        generation: u16,
    },
}

type XrefMap = FxHashMap<ObjectIdentifier, EntryType>;

/// Representation of a proper xref table.
#[derive(Debug)]
struct MapRepr {
    xref_map: XrefMap,
    repaired: bool,
}

#[derive(Debug, Copy, Clone)]
pub(crate) struct TrailerData {
    pub(crate) pages_ref: ObjectIdentifier,
    pub(crate) root_ref: ObjectIdentifier,
    pub(crate) version: Option<PdfVersion>,
}

impl TrailerData {
    pub(crate) fn dummy() -> Self {
        Self {
            pages_ref: ObjectIdentifier::new(0, 0),
            root_ref: ObjectIdentifier::new(0, 0),
            version: None,
        }
    }
}

#[derive(Debug, Clone)]
struct SomeRepr {
    data: Arc<Data>,
    map: Arc<RwLock<MapRepr>>,
    metadata: Arc<Metadata>,
    decryptor: Arc<Decryptor>,
    has_ocgs: bool,
    password: Vec<u8>,
    trailer_data: TrailerData,
    /// Raw bytes of the trailer dictionary, retained so that
    /// [`XRef::trailer`] can re-parse the full trailer on demand. `None`
    /// when the xref was built from a [`XRefInput::RootRef`] fallback,
    /// i.e. the original trailer could not be read.
    trailer_dict_bytes: Option<Arc<[u8]>>,
    /// For a streaming source only: every `Normal` object's byte offset,
    /// sorted ascending, with the file length appended as a sentinel. Used
    /// to bound each object's on-demand read at the next object's start -
    /// objects are non-overlapping, so `[offset, next)` contains the whole
    /// object. Empty for a resident source.
    sorted_offsets: Vec<u64>,
}

impl SomeRepr {
    /// The exclusive end of the byte window for the streamed object at
    /// `offset`: the next `Normal` offset strictly greater than `offset`, or
    /// the file length. See [`SomeRepr::sorted_offsets`].
    fn next_object_bound(&self, offset: usize) -> u64 {
        let off = offset as u64;
        let idx = self.sorted_offsets.partition_point(|&o| o <= off);
        self.sorted_offsets.get(idx).copied().unwrap_or(off)
    }
}

#[derive(Debug, Clone)]
enum Inner {
    /// A dummy xref table that doesn't have any entries.
    Dummy,
    /// A proper xref table.
    Some(Arc<SomeRepr>),
}

#[derive(Debug)]
struct XRefEntry {
    offset: usize,
    gen_number: i32,
    used: bool,
}

impl XRefEntry {
    pub(crate) fn read(data: &[u8]) -> Option<Self> {
        #[inline(always)]
        fn parse_u32(data: &[u8]) -> Option<u32> {
            let mut accum = 0_u32;

            for byte in data {
                accum = accum.checked_mul(10)?;

                match *byte {
                    b'0'..=b'9' => accum = accum.checked_add((*byte - b'0') as u32)?,
                    _ => return None,
                }
            }

            Some(accum)
        }

        let offset = parse_u32(&data[0..10])? as usize;
        let gen_number = i32::try_from(parse_u32(&data[11..16])?).ok()?;

        let used = data[17] == b'n';

        Some(Self {
            offset,
            gen_number,
            used,
        })
    }
}

fn populate_xref_impl<'a>(data: &'a [u8], pos: usize, xref_map: &mut XrefMap) -> Option<&'a [u8]> {
    let mut visited = BTreeSet::new();
    populate_xref_impl_inner(data, pos, xref_map, &mut visited)
}

/// Maximum number of allowed xref `Prev` pointers before we abort.
const MAX_XREF_CHAIN_DEPTH: usize = 256;

fn populate_xref_impl_inner<'a>(
    data: &'a [u8],
    pos: usize,
    xref_map: &mut XrefMap,
    visited: &mut BTreeSet<usize>,
) -> Option<&'a [u8]> {
    if !visited.insert(pos) {
        warn!("circular xref PREV chain detected at offset {}", pos);

        return None;
    }

    if visited.len() > MAX_XREF_CHAIN_DEPTH {
        warn!(
            "xref PREV chain exceeds maximum depth of {}",
            MAX_XREF_CHAIN_DEPTH
        );

        return None;
    }

    let mut reader = Reader::new(data);
    reader.jump(pos);
    // In case the position points to before the object number of a xref stream.
    reader.skip_white_spaces_and_comments();

    let mut r2 = reader.clone();
    if reader
        .clone()
        .read_without_context::<ObjectIdentifier>()
        .is_some()
    {
        populate_from_xref_stream(data, &mut r2, xref_map, visited)
    } else {
        populate_from_xref_table(data, &mut r2, xref_map, visited)
    }
}

pub(super) struct SubsectionHeader {
    pub(super) start: u32,
    pub(super) num_entries: u32,
}

impl Readable<'_> for SubsectionHeader {
    fn read(r: &mut Reader<'_>, _: &ReaderContext<'_>) -> Option<Self> {
        r.skip_white_spaces();
        let start = r.read_without_context::<u32>()?;
        r.skip_white_spaces();
        let num_entries = r.read_without_context::<u32>()?;
        r.skip_white_spaces();

        Some(Self { start, num_entries })
    }
}

/// Populate the xref table, and return the trailer dict.
fn populate_from_xref_table<'a>(
    data: &'a [u8],
    reader: &mut Reader<'a>,
    insert_map: &mut XrefMap,
    visited: &mut BTreeSet<usize>,
) -> Option<&'a [u8]> {
    let trailer = {
        let mut reader = reader.clone();
        read_xref_table_trailer(&mut reader, &ReaderContext::dummy())?
    };

    reader.skip_white_spaces();
    reader.forward_tag(b"xref")?;
    reader.skip_white_spaces();

    let mut max_obj = 0;

    if let Some(prev) = trailer.get::<i32>(PREV) {
        // First insert the entries from any previous xref tables.
        populate_xref_impl_inner(data, prev as usize, insert_map, visited)?;
    }

    while let Some(header) = reader.read_without_context::<SubsectionHeader>() {
        reader.skip_white_spaces();

        let start = header.start;
        let end = start + header.num_entries;

        for obj_number in start..end {
            max_obj = max(max_obj, obj_number);
            let bytes = reader.read_bytes(XREF_ENTRY_LEN)?;
            let entry = XRefEntry::read(bytes)?;

            // Specification says we should ignore any object number > SIZE, but probably
            // not important?
            if entry.used {
                insert_map.insert(
                    ObjectIdentifier::new(obj_number as i32, entry.gen_number),
                    EntryType::Normal { offset: entry.offset },
                );
            } else {
                // Free entry: the "offset" column is the next free object
                // number, the "generation" column is the generation to use
                // when this slot is re-used. Per §7.5.4, the canonical head
                // of the free list is at object 0 with generation 65535.
                let next_free = i32::try_from(entry.offset).unwrap_or(i32::MAX);
                let generation = u16::try_from(entry.gen_number).unwrap_or(u16::MAX);
                insert_map.insert(
                    ObjectIdentifier::new(obj_number as i32, entry.gen_number),
                    EntryType::Free {
                        next_free,
                        generation,
                    },
                );
            }
        }
    }

    // In hybrid files, entries in `XRefStm` must override the classic xref
    // table entries for the same revision (PDF 32000-1 § 7.5.8.4): the classic
    // table is a legacy-reader fallback that pre-dates cross-reference
    // streams, and the spec instructs PDF 1.5+ readers to prefer the stream's
    // entries. Insert after `PREV` (older revisions) AND after the current
    // classic table so the stream's entries win for this revision.
    if let Some(xref_stm) = trailer.get::<i32>(XREF_STM) {
        // Hybrid files often share a single `/XRefStm` across multiple
        // revision trailers (each later revision's incremental update
        // re-publishes the same `/XRefStm` pointer rather than authoring
        // a new stream). The first traversal inserts the stream's entries
        // into the visited set; subsequent visits MUST skip silently rather
        // than propagate the cycle-guard's `None` upward, which would abort
        // the whole xref build and force a fallback scan.
        let _ = populate_xref_impl_inner(data, xref_stm as usize, insert_map, visited);
    }

    Some(trailer.data())
}

fn populate_from_xref_stream<'a>(
    data: &'a [u8],
    reader: &mut Reader<'a>,
    insert_map: &mut XrefMap,
    visited: &mut BTreeSet<usize>,
) -> Option<&'a [u8]> {
    let stream = reader
        .read_with_context::<IndirectObject<Stream<'_>>>(&ReaderContext::dummy())?
        .get();

    if let Some(prev) = stream.dict().get::<i32>(PREV) {
        // First insert the entries from any previous xref tables.
        let _ = populate_xref_impl_inner(data, prev as usize, insert_map, visited)?;
    }

    let size = stream.dict().get::<u32>(SIZE)?;

    let [f1_len, f2_len, f3_len] = stream.dict().get::<[u8; 3]>(W)?;

    if f2_len > size_of::<u64>() as u8 {
        error!("xref offset length is larger than the allowed limit");

        return None;
    }

    // Do such files exist?
    if f1_len != 1 {
        warn!("first field in xref stream was longer than 1");
    }

    let xref_data = stream.decoded().ok()?;
    let mut xref_reader = Reader::new(xref_data.as_ref());

    if let Some(arr) = stream.dict().get::<Array<'_>>(INDEX) {
        let iter = arr.iter::<(u32, u32)>();

        for (start, num_elements) in iter {
            xref_stream_subsection(
                &mut xref_reader,
                start,
                num_elements,
                f1_len,
                f2_len,
                f3_len,
                insert_map,
            )?;
        }
    } else {
        xref_stream_subsection(
            &mut xref_reader,
            0,
            size,
            f1_len,
            f2_len,
            f3_len,
            insert_map,
        )?;
    }

    Some(stream.dict().data())
}

fn xref_stream_num(data: &[u8]) -> Option<u32> {
    Some(match data.len() {
        0 => return None,
        1 => u8::from_be(data[0]) as u32,
        2 => u16::from_be_bytes(data[0..2].try_into().ok()?) as u32,
        3 => u32::from_be_bytes([0, data[0], data[1], data[2]]),
        4 => u32::from_be_bytes(data[0..4].try_into().ok()?),
        8 => {
            if let Ok(num) = u32::try_from(u64::from_be_bytes(data[0..8].try_into().ok()?)) {
                return Some(num);
            } else {
                warn!("xref stream number is too large");

                return None;
            }
        }
        _n => {
            warn!("invalid xref stream number {_n}");

            return None;
        }
    })
}

fn xref_stream_subsection<'a>(
    xref_reader: &mut Reader<'a>,
    start: u32,
    num_elements: u32,
    f1_len: u8,
    f2_len: u8,
    f3_len: u8,
    insert_map: &mut XrefMap,
) -> Option<()> {
    for i in 0..num_elements {
        let f_type = if f1_len == 0 {
            1
        } else {
            // We assume a length of 1.
            xref_reader.read_bytes(1)?[0]
        };

        let obj_number = start + i;

        match f_type {
            0 => {
                // Free entry: field 2 is the next-free object number,
                // field 3 is the generation for next re-use.
                let next_free = if f2_len > 0 {
                    let data = xref_reader.read_bytes(f2_len as usize)?;
                    xref_stream_num(data)?
                } else {
                    0
                };

                let gen_number = if f3_len > 0 {
                    let data = xref_reader.read_bytes(f3_len as usize)?;
                    xref_stream_num(data)?
                } else {
                    0
                };

                insert_map.insert(
                    ObjectIdentifier::new(obj_number as i32, gen_number as i32),
                    EntryType::Free {
                        next_free: i32::try_from(next_free).unwrap_or(i32::MAX),
                        generation: u16::try_from(gen_number).unwrap_or(u16::MAX),
                    },
                );
            }
            1 => {
                let offset = if f2_len > 0 {
                    let data = xref_reader.read_bytes(f2_len as usize)?;
                    xref_stream_num(data)?
                } else {
                    0
                };

                let gen_number = if f3_len > 0 {
                    let data = xref_reader.read_bytes(f3_len as usize)?;
                    xref_stream_num(data)?
                } else {
                    0
                };

                insert_map.insert(
                    ObjectIdentifier::new(obj_number as i32, gen_number as i32),
                    EntryType::Normal {
                        offset: offset as usize,
                    },
                );
            }
            2 => {
                let obj_stream_number = {
                    let data = xref_reader.read_bytes(f2_len as usize)?;
                    xref_stream_num(data)?
                };
                let gen_number = 0;
                let index = if f3_len > 0 {
                    let data = xref_reader.read_bytes(f3_len as usize)?;
                    xref_stream_num(data)?
                } else {
                    0
                };

                insert_map.insert(
                    ObjectIdentifier::new(obj_number as i32, gen_number),
                    EntryType::Compressed {
                        obj_stream: obj_stream_number as i32,
                        index,
                    },
                );
            }
            _ => {
                warn!("xref has unknown field type {f_type}");

                return None;
            }
        }
    }

    Some(())
}

fn read_xref_table_trailer<'a>(
    reader: &mut Reader<'a>,
    ctx: &ReaderContext<'a>,
) -> Option<Dict<'a>> {
    reader.skip_white_spaces();
    reader.forward_tag(b"xref")?;
    reader.skip_white_spaces();

    while let Some(header) = reader.read_without_context::<SubsectionHeader>() {
        reader.jump(reader.offset() + XREF_ENTRY_LEN * header.num_entries as usize);
    }

    reader.skip_white_spaces();
    reader.forward_tag(b"trailer")?;
    reader.skip_white_spaces();

    reader.read_with_context::<Dict<'_>>(ctx)
}

fn get_decryptor(trailer_dict: &Dict<'_>, password: &[u8]) -> Result<Decryptor, XRefError> {
    if let Some(encryption_dict) = trailer_dict.get::<Dict<'_>>(ENCRYPT) {
        let id = if let Some(id) = trailer_dict
            .get::<Array<'_>>(ID)
            .and_then(|a| a.flex_iter().next::<object::String<'_>>())
        {
            id.to_vec()
        } else {
            // Assume an empty ID entry.
            vec![]
        };

        get(&encryption_dict, &id, password).map_err(XRefError::Encryption)
    } else {
        Ok(Decryptor::None)
    }
}

struct ObjectStream<'a> {
    data: &'a [u8],
    ctx: ReaderContext<'a>,
    offsets: Vec<(u32, usize)>,
}

impl<'a> ObjectStream<'a> {
    fn new(inner: Stream<'_>, data: &'a [u8], ctx: &ReaderContext<'a>) -> Option<Self> {
        let num_objects = inner.dict().get::<usize>(N)?;
        let first_offset = inner.dict().get::<usize>(FIRST)?;

        let mut r = Reader::new(data);

        let mut offsets = vec![];

        for _ in 0..num_objects {
            r.skip_white_spaces_and_comments();
            // Skip object number
            let obj_num = r.read_without_context::<u32>()?;
            r.skip_white_spaces_and_comments();
            let relative_offset = r.read_without_context::<usize>()?;
            offsets.push((obj_num, first_offset + relative_offset));
        }

        let mut ctx = ctx.clone();
        ctx.set_in_object_stream(true);

        Some(Self { data, ctx, offsets })
    }

    fn get<T>(&self, index: u32) -> Option<T>
    where
        T: ObjectLike<'a>,
    {
        let offset = self.offsets.get(index as usize)?.1;
        let mut r = Reader::new(self.data);
        r.jump(offset);
        r.skip_white_spaces_and_comments();

        r.read_with_context::<T>(&self.ctx)
    }
}

fn parse_metadata(info_dict: &Dict<'_>) -> Metadata {
    Metadata {
        creation_date: info_dict
            .get::<object::String<'_>>(CREATION_DATE)
            .and_then(|c| DateTime::from_bytes(&c)),
        modification_date: info_dict
            .get::<object::String<'_>>(MOD_DATE)
            .and_then(|c| DateTime::from_bytes(&c)),
        title: info_dict
            .get::<object::String<'_>>(TITLE)
            .map(|t| t.to_vec()),
        author: info_dict
            .get::<object::String<'_>>(AUTHOR)
            .map(|t| t.to_vec()),
        subject: info_dict
            .get::<object::String<'_>>(SUBJECT)
            .map(|t| t.to_vec()),
        keywords: info_dict
            .get::<object::String<'_>>(KEYWORDS)
            .map(|t| t.to_vec()),
        creator: info_dict
            .get::<object::String<'_>>(CREATOR)
            .map(|t| t.to_vec()),
        producer: info_dict
            .get::<object::String<'_>>(PRODUCER)
            .map(|t| t.to_vec()),
    }
}

/// The serialisation form of a cross-reference section.
///
/// ISO 32000-1 §7.5.4 describes the classical `xref` keyword table;
/// §7.5.8 introduced xref streams in PDF 1.5.
///
/// Requires the `inspect` feature.
#[cfg(feature = "inspect")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum XRefKind {
    /// Classical `xref` keyword + subsection headers (§7.5.4).
    Table,
    /// Xref stream object (§7.5.8).
    Stream,
}

/// A single cross-reference section in the file.
///
/// A document may have multiple sections if incremental updates were
/// used (§7.5.6). Returned by [`XRef::sections`] in
/// `startxref → /Prev` walk order (most-recent-first).
///
/// Requires the `inspect` feature.
#[cfg(feature = "inspect")]
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct XRefSection {
    /// Byte offset of the section, at the `xref` keyword or the xref
    /// stream object's `N G obj` header.
    pub offset: usize,
    /// Serialisation form of this section.
    pub kind: XRefKind,
    /// For [`XRefKind::Table`]: byte ranges of each subsection header
    /// line (e.g. `0 4`) in file order. Empty for xref streams.
    pub subsection_headers: Vec<core::ops::Range<usize>>,
    /// Byte offset of the `xref` keyword (for `Table`) or the stream
    /// object's `N G obj` header (for `Stream`). Equal to `offset`.
    pub keyword_offset: usize,
    /// Byte following the terminator of the section: the position
    /// after the trailer dict for `Table`, or after `endobj` for
    /// `Stream`.
    pub end_offset: usize,
}

/// Walk the `startxref` + `/Prev` chain starting at `pos`, collecting
/// section metadata. Silent on malformed sections (just stops).
#[cfg(feature = "inspect")]
fn collect_sections(
    data: &[u8],
    pos: usize,
    out: &mut Vec<XRefSection>,
    visited: &mut BTreeSet<usize>,
) {
    if !visited.insert(pos) {
        return;
    }
    let Some(tail) = data.get(pos..) else {
        return;
    };

    if tail.starts_with(b"xref") {
        if let Some((section, prev)) = scan_table_section(data, pos) {
            out.push(section);
            if let Some(prev_pos) = prev {
                collect_sections(data, prev_pos, out, visited);
            }
        }
    } else {
        // Xref stream — the position should be at a `N G obj` header.
        if let Some((section, prev)) = scan_stream_section(data, pos) {
            out.push(section);
            if let Some(prev_pos) = prev {
                collect_sections(data, prev_pos, out, visited);
            }
        }
    }
}

/// Scan a classical xref table section. Returns the section metadata
/// and the `/Prev` offset if present.
#[cfg(feature = "inspect")]
fn scan_table_section(data: &[u8], pos: usize) -> Option<(XRefSection, Option<usize>)> {
    let mut reader = Reader::new(data);
    reader.jump(pos);
    reader.forward_tag(b"xref")?;
    reader.skip_white_spaces();

    let mut subsection_headers: Vec<core::ops::Range<usize>> = Vec::new();

    loop {
        reader.skip_white_spaces();
        let header_start = reader.offset();
        let header = match reader.read_without_context::<SubsectionHeader>() {
            Some(h) => h,
            None => break,
        };
        let header_end = reader.offset();
        subsection_headers.push(header_start..header_end);

        // Skip the entry rows for this subsection.
        let rows_len = XREF_ENTRY_LEN * header.num_entries as usize;
        reader.jump(reader.offset() + rows_len);
    }

    reader.skip_white_spaces();
    reader.forward_tag(b"trailer")?;
    reader.skip_white_spaces();

    let trailer_start = reader.offset();
    let trailer_dict = reader.read_with_context::<Dict<'_>>(&ReaderContext::dummy())?;
    let trailer_end = trailer_start + trailer_dict.data().len();

    let prev = trailer_dict.get::<i32>(PREV).and_then(|p| {
        if p >= 0 {
            Some(p as usize)
        } else {
            None
        }
    });

    Some((
        XRefSection {
            offset: pos,
            kind: XRefKind::Table,
            subsection_headers,
            keyword_offset: pos,
            end_offset: trailer_end,
        },
        prev,
    ))
}

/// Scan an xref-stream section. Returns the section metadata and the
/// `/Prev` offset if present.
#[cfg(feature = "inspect")]
fn scan_stream_section(data: &[u8], pos: usize) -> Option<(XRefSection, Option<usize>)> {
    let layout = crate::layout::scan_indirect_layout(data, pos)?;
    let end_offset = layout.range.end;

    let mut reader = Reader::new(data);
    reader.jump(pos);
    let stream = reader
        .read_with_context::<IndirectObject<Stream<'_>>>(&ReaderContext::dummy())?
        .get();
    let prev = stream.dict().get::<i32>(PREV).and_then(|p| {
        if p >= 0 {
            Some(p as usize)
        } else {
            None
        }
    });

    Some((
        XRefSection {
            offset: pos,
            kind: XRefKind::Stream,
            subsection_headers: Vec::new(),
            keyword_offset: pos,
            end_offset,
        },
        prev,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Pdf;
    use crate::object::Array;

    /// Build a minimal valid PDF with the given object bodies and trailer
    /// entries, computing the xref offsets automatically. Object numbers
    /// start at 1 and are consecutive.
    fn build_pdf(objects: &[&str], trailer_entries: &str) -> Vec<u8> {
        let mut pdf: Vec<u8> = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");

        let mut offsets: Vec<usize> = Vec::with_capacity(objects.len());
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            pdf.extend_from_slice(body.as_bytes());
            pdf.extend_from_slice(b"\nendobj\n");
        }

        let xref_pos = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(b"trailer\n");
        pdf.extend_from_slice(
            format!(
                "<< /Size {size} {entries} >>\n",
                size = objects.len() + 1,
                entries = trailer_entries,
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF").as_bytes());
        pdf
    }

    #[test]
    fn trailer_returns_some_and_carries_id_array() {
        // Catalog + Pages + document-info, with /ID in the trailer.
        let bytes = build_pdf(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [] /Count 0 >>",
                "<< /Title (Test) >>",
            ],
            "/Root 1 0 R /Info 3 0 R /ID [<abcdef> <012345>]",
        );
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let trailer = pdf.trailer().expect("trailer available");
        let id = trailer.get::<Array<'_>>(b"ID").expect("ID array");
        assert_eq!(id.iter::<object::String<'_>>().count(), 2);
    }

    #[test]
    fn trailer_resolves_info_indirect_ref() {
        let bytes = build_pdf(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [] /Count 0 >>",
                "<< /Title (Doc) >>",
            ],
            "/Root 1 0 R /Info 3 0 R",
        );
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let trailer = pdf.trailer().expect("trailer available");
        let info_ref = trailer.get_ref(b"Info").expect("Info ref");
        assert_eq!(info_ref.obj_number, 3);
        assert_eq!(info_ref.gen_number, 0);
    }

    #[test]
    fn trailer_exposes_size() {
        let bytes = build_pdf(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [] /Count 0 >>",
            ],
            "/Root 1 0 R",
        );
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let trailer = pdf.trailer().expect("trailer available");
        // Four entries: the null free-list entry plus three objects.
        let size: i32 = trailer.get(b"Size").expect("Size integer");
        assert_eq!(size, 3);
    }

    #[test]
    fn trailer_on_dummy_xref_returns_none() {
        let dummy = XRef::dummy();
        assert!(dummy.trailer().is_none());
    }

    #[test]
    fn trailer_parses_same_dict_on_repeated_calls() {
        let bytes = build_pdf(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [] /Count 0 >>",
            ],
            "/Root 1 0 R /ID [<aa> <bb>]",
        );
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let first = pdf.trailer().expect("trailer available");
        let second = pdf.trailer().expect("trailer available");
        // Re-parse must be deterministic: same keys on both calls.
        let k1: Vec<Vec<u8>> = first.keys().map(|k| k.as_ref().to_vec()).collect();
        let k2: Vec<Vec<u8>> = second.keys().map(|k| k.as_ref().to_vec()).collect();
        assert_eq!(k1, k2);
    }

    #[test]
    fn encryption_dict_absent_for_inline_unencrypted_pdf() {
        let bytes = build_pdf(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [] /Count 0 >>",
            ],
            "/Root 1 0 R",
        );
        let pdf = Pdf::new(bytes).expect("pdf loads");
        assert!(!pdf.is_encrypted());
        assert!(pdf.encryption_dict().is_none());
    }

    #[test]
    fn encryption_dict_on_dummy_xref_is_none() {
        let dummy = XRef::dummy();
        assert!(!dummy.is_encrypted());
        assert!(dummy.encryption_dict().is_none());
    }

    #[test]
    fn encryption_dict_present_for_aes_128_fixture() {
        let bytes: &[u8] = include_bytes!("../../hayro-tests/pdfs/custom/encrypted_aes_128.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("aes-128 fixture loads");
        assert!(pdf.is_encrypted());
        let enc = pdf.encryption_dict().expect("encryption dict");
        let filter = enc.get::<Name<'_>>(b"Filter").expect("Filter name");
        assert_eq!(filter.deref(), b"Standard");
    }

    #[test]
    fn encryption_dict_present_for_aes_256_fixture() {
        let bytes: &[u8] = include_bytes!("../../hayro-tests/pdfs/custom/encrypted_aes_256.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("aes-256 fixture loads");
        assert!(pdf.is_encrypted());
        let enc = pdf.encryption_dict().expect("encryption dict");
        // AES-256 uses V >= 5 and R >= 5 per ISO 32000-2.
        let v: i32 = enc.get(b"V").expect("V integer");
        let r: i32 = enc.get(b"R").expect("R integer");
        assert!(v >= 5, "V = {v}");
        assert!(r >= 5, "R = {r}");
    }

    #[test]
    fn encryption_dict_absent_for_unencrypted_real_fixture() {
        let bytes: &[u8] =
            include_bytes!("../../hayro-tests/pdfs/custom/andler-optimal-lot-size.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("fixture loads");
        assert!(!pdf.is_encrypted());
        assert!(pdf.encryption_dict().is_none());
    }

    #[test]
    fn encryption_dict_present_after_password_decryption() {
        let bytes: &[u8] =
            include_bytes!("../../hayro-tests/pdfs/custom/password_encrypted_aes_128.pdf");
        let pdf = Pdf::new_with_password(bytes.to_vec(), "testpw")
            .expect("password-protected fixture decrypts");
        // /Encrypt is still present in the trailer even after successful decryption.
        assert!(pdf.is_encrypted());
        let enc = pdf.encryption_dict().expect("encryption dict");
        let filter = enc.get::<Name<'_>>(b"Filter").expect("Filter name");
        assert_eq!(filter.deref(), b"Standard");
    }

    #[test]
    fn trailer_on_real_fixture_has_size_and_root() {
        // Tier B: committed fixture.
        let bytes: &[u8] =
            include_bytes!("../../hayro-tests/pdfs/custom/andler-optimal-lot-size.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("fixture loads");
        let trailer = pdf.trailer().expect("trailer available");
        let size: i32 = trailer.get(b"Size").expect("Size integer");
        assert!(size > 0);
        let root_ref = trailer.get_ref(b"Root").expect("Root ref");
        assert_eq!(root_ref, pdf.xref().root_id().into());
    }

    // --- PR #7: entry types, entries iteration, size --------------------

    /// Build a valid PDF with a crafted xref whose free list chains two
    /// entries:
    ///
    ///   0: free head → 3, generation 65535
    ///   1: in-use  (catalog) at offset `off1`
    ///   2: in-use  (pages)   at offset `off2`
    ///   3: free         → 0 (terminator), generation 1
    fn build_pdf_with_free_list() -> (Vec<u8>, usize, usize) {
        let catalog = "<< /Type /Catalog /Pages 2 0 R >>";
        let pages = "<< /Type /Pages /Kids [] /Count 0 >>";

        let mut pdf: Vec<u8> = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.7\n");
        let off1 = pdf.len();
        pdf.extend_from_slice(format!("1 0 obj\n{catalog}\nendobj\n").as_bytes());
        let off2 = pdf.len();
        pdf.extend_from_slice(format!("2 0 obj\n{pages}\nendobj\n").as_bytes());

        let xref_pos = pdf.len();
        pdf.extend_from_slice(b"xref\n0 4\n");
        pdf.extend_from_slice(b"0000000003 65535 f \n"); // obj 0 -> free, next_free = 3
        pdf.extend_from_slice(format!("{off1:010} 00000 n \n").as_bytes()); // obj 1 in use
        pdf.extend_from_slice(format!("{off2:010} 00000 n \n").as_bytes()); // obj 2 in use
        pdf.extend_from_slice(b"0000000000 00001 f \n"); // obj 3 free, terminator, gen 1
        pdf.extend_from_slice(b"trailer\n<< /Size 4 /Root 1 0 R >>\n");
        pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF").as_bytes());
        (pdf, off1, off2)
    }

    #[test]
    fn entries_include_free_entries() {
        let (bytes, _, _) = build_pdf_with_free_list();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let entries = pdf.xref().entries();
        let free_count = entries
            .iter()
            .filter(|(_, e)| matches!(e, EntryType::Free { .. }))
            .count();
        assert_eq!(free_count, 2, "expected two free entries");
    }

    #[test]
    fn entry_resolves_canonical_head_of_free_list() {
        let (bytes, _, _) = build_pdf_with_free_list();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let head = pdf
            .xref()
            .entry(ObjectIdentifier::new(0, 65535))
            .expect("free head present");
        match head {
            EntryType::Free { next_free, generation } => {
                assert_eq!(next_free, 3);
                assert_eq!(generation, 65535);
            }
            other => panic!("expected Free, got {other:?}"),
        }
    }

    #[test]
    fn entry_resolves_free_list_chain_member() {
        let (bytes, _, _) = build_pdf_with_free_list();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let entry = pdf
            .xref()
            .entry(ObjectIdentifier::new(3, 1))
            .expect("obj 3 present");
        assert_eq!(
            entry,
            EntryType::Free {
                next_free: 0,
                generation: 1
            }
        );
    }

    #[test]
    fn entry_resolves_normal_at_correct_offset() {
        let (bytes, off1, off2) = build_pdf_with_free_list();
        let pdf = Pdf::new(bytes).expect("pdf loads");

        let e1 = pdf.xref().entry(ObjectIdentifier::new(1, 0)).unwrap();
        assert_eq!(e1, EntryType::Normal { offset: off1 });

        let e2 = pdf.xref().entry(ObjectIdentifier::new(2, 0)).unwrap();
        assert_eq!(e2, EntryType::Normal { offset: off2 });
    }

    #[test]
    fn size_reports_trailer_value() {
        let (bytes, _, _) = build_pdf_with_free_list();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        assert_eq!(pdf.xref().size(), 4);
    }

    #[test]
    fn entry_returns_none_for_unknown_id() {
        let (bytes, _, _) = build_pdf_with_free_list();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        assert!(pdf.xref().entry(ObjectIdentifier::new(999, 0)).is_none());
    }

    #[test]
    fn entry_on_dummy_xref_is_none() {
        let dummy = XRef::dummy();
        assert!(dummy.entry(ObjectIdentifier::new(1, 0)).is_none());
        assert!(dummy.entries().is_empty());
        assert_eq!(dummy.size(), 0);
    }

    // --- PR #8: xref sections -------------------------------------------

    #[cfg(feature = "inspect")]
    #[test]
    fn sections_for_inline_table() {
        let bytes = build_pdf(
            &[
                "<< /Type /Catalog /Pages 2 0 R >>",
                "<< /Type /Pages /Kids [] /Count 0 >>",
            ],
            "/Root 1 0 R",
        );
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let sections = pdf.xref().sections();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].kind, XRefKind::Table);
        assert!(!sections[0].subsection_headers.is_empty());
        assert!(sections[0].end_offset > sections[0].offset);
        assert_eq!(sections[0].keyword_offset, sections[0].offset);
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn sections_on_dummy_xref_is_empty() {
        let dummy = XRef::dummy();
        assert!(dummy.sections().is_empty());
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn sections_on_real_fixture_has_at_least_one() {
        let bytes: &[u8] =
            include_bytes!("../../hayro-tests/pdfs/custom/andler-optimal-lot-size.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("fixture loads");
        let sections = pdf.xref().sections();
        assert!(!sections.is_empty());
        for section in &sections {
            assert!(section.offset < section.end_offset);
            assert_eq!(section.keyword_offset, section.offset);
            if section.kind == XRefKind::Table {
                assert!(!section.subsection_headers.is_empty());
            }
        }
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn sections_for_xref_stream_fixture() {
        // catalog-in-objstm-aes uses a 1.5+ layout with an xref stream.
        let bytes: &[u8] =
            include_bytes!("../../hayro-tests/pdfs/custom/catalog-in-objstm-aes.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("fixture loads");
        let sections = pdf.xref().sections();
        assert!(!sections.is_empty());
        let has_stream = sections.iter().any(|s| s.kind == XRefKind::Stream);
        assert!(
            has_stream,
            "catalog-in-objstm-aes must contain at least one xref stream section"
        );
        for section in &sections {
            if section.kind == XRefKind::Stream {
                assert!(section.subsection_headers.is_empty());
            }
        }
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn sections_incremental_update_most_recent_first() {
        // Build a PDF with one base section + one update section chained
        // via /Prev. The updated trailer is placed at the end of the file.
        let catalog = "<< /Type /Catalog /Pages 2 0 R >>";
        let pages = "<< /Type /Pages /Kids [] /Count 0 >>";

        let mut pdf: Vec<u8> = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.7\n");
        let off1 = pdf.len();
        pdf.extend_from_slice(format!("1 0 obj\n{catalog}\nendobj\n").as_bytes());
        let off2 = pdf.len();
        pdf.extend_from_slice(format!("2 0 obj\n{pages}\nendobj\n").as_bytes());

        // Original xref.
        let xref_a_pos = pdf.len();
        pdf.extend_from_slice(b"xref\n0 3\n");
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        pdf.extend_from_slice(format!("{off1:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off2:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(b"trailer\n<< /Size 3 /Root 1 0 R >>\n");
        pdf.extend_from_slice(format!("startxref\n{xref_a_pos}\n%%EOF\n").as_bytes());

        // Incremental update: add object 3, new xref section chained to prev.
        let off3 = pdf.len();
        pdf.extend_from_slice(b"3 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        let xref_b_pos = pdf.len();
        pdf.extend_from_slice(b"xref\n3 1\n");
        pdf.extend_from_slice(format!("{off3:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 4 /Root 1 0 R /Prev {xref_a_pos} >>\n").as_bytes(),
        );
        pdf.extend_from_slice(format!("startxref\n{xref_b_pos}\n%%EOF").as_bytes());

        let pdf_obj = Pdf::new(pdf).expect("pdf loads");
        let sections = pdf_obj.xref().sections();
        assert_eq!(sections.len(), 2, "{sections:?}");
        // Most-recent-first: the later xref (higher offset) comes first.
        assert!(
            sections[0].offset > sections[1].offset,
            "sections must be ordered most-recent-first"
        );
        assert_eq!(sections[0].kind, XRefKind::Table);
        assert_eq!(sections[1].kind, XRefKind::Table);
    }

    // --- PR #9: indirect layout -----------------------------------------

    #[cfg(feature = "inspect")]
    #[test]
    fn indirect_layout_direct_canonical() {
        use crate::layout::LayoutKind;

        let (bytes, off1, _) = build_pdf_with_free_list();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let kind = pdf
            .xref()
            .indirect_layout(ObjectIdentifier::new(1, 0))
            .expect("layout for obj 1");
        match kind {
            LayoutKind::Direct(layout) => {
                assert_eq!(layout.range.start, off1);
                assert!(layout.header_canonical);
                assert!(layout.endobj_preceded_by_eol);
                assert!(layout.endobj_followed_by_eol);
            }
            other => panic!("expected Direct, got {other:?}"),
        }
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn indirect_layout_free_entry() {
        use crate::layout::LayoutKind;

        let (bytes, _, _) = build_pdf_with_free_list();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let kind = pdf
            .xref()
            .indirect_layout(ObjectIdentifier::new(3, 1))
            .expect("layout for free obj");
        assert_eq!(kind, LayoutKind::Free);
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn indirect_layout_absent_id_is_none() {
        let (bytes, _, _) = build_pdf_with_free_list();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        assert!(
            pdf.xref()
                .indirect_layout(ObjectIdentifier::new(99, 0))
                .is_none()
        );
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn indirect_layout_on_dummy_xref_is_none() {
        let dummy = XRef::dummy();
        assert!(
            dummy
                .indirect_layout(ObjectIdentifier::new(1, 0))
                .is_none()
        );
    }

    #[cfg(feature = "inspect")]
    #[test]
    fn indirect_layout_compressed_via_real_fixture() {
        use crate::layout::LayoutKind;

        let bytes: &[u8] =
            include_bytes!("../../hayro-tests/pdfs/custom/catalog-in-objstm-aes.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("fixture loads");
        let catalog_id = pdf.xref().root_id();
        let kind = pdf
            .xref()
            .indirect_layout(catalog_id)
            .expect("layout for catalog");
        match kind {
            LayoutKind::Compressed {
                host,
                host_layout,
                index: _,
            } => {
                // The host stream itself must be direct (never compressed).
                let host_kind = pdf.xref().indirect_layout(host).expect("host layout");
                assert!(matches!(host_kind, LayoutKind::Direct(_)));
                assert!(host_layout.range.start < host_layout.range.end);
            }
            other => panic!("expected Compressed catalog, got {other:?}"),
        }
    }

    #[test]
    fn entries_on_real_fixture_matches_size() {
        let bytes: &[u8] =
            include_bytes!("../../hayro-tests/pdfs/custom/andler-optimal-lot-size.pdf");
        let pdf = Pdf::new(bytes.to_vec()).expect("fixture loads");
        let xref = pdf.xref();
        let size = xref.size();
        let count = xref.entries().len();
        // Every object number up to /Size is accounted for as either an
        // in-use entry or a free entry on the free list.
        assert!(
            count <= size as usize,
            "entries ({count}) <= size ({size})"
        );
    }

    #[test]
    fn circular_prev_chain() {
        let mut pdf = b"%PDF-1.0\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_vec();
        let expected_xref_pos = pdf.len();
        pdf.extend_from_slice(
            format!(
                "xref\n\
                 0 1\n\
                 0000000000 65535 f \r\n\
                 trailer\n<< /Size 1 /Root 1 0 R /Prev {expected_xref_pos} >>\n\
                 startxref\n{expected_xref_pos}\n%%EOF"
            )
            .as_bytes(),
        );

        let mut xref_map = FxHashMap::default();
        let xref_pos = find_last_xref_pos(pdf.as_ref()).unwrap();
        let _result = populate_xref_impl(pdf.as_ref(), xref_pos, &mut xref_map);
    }

    #[test]
    fn find_last_xref_uses_last_startxref() {
        let pdf = b"%PDF-1.0\nstartxref\n5\n%%EOF\nstartxref\n42\n%%EOF";
        assert_eq!(find_last_xref_pos(pdf), Some(42));
    }

    /// A shared `/XRefStm` referenced by multiple revision trailers must not
    /// abort the xref build via the cycle guard. Real-world incremental
    /// updates routinely re-publish the original revision's `/XRefStm`
    /// pointer in the new trailer (e.g. signing tools that update `/Prev`
    /// + `/XRefStm` in lockstep). The first traversal visits the stream
    /// offset; subsequent visits return `None` from `populate_xref_impl_inner`,
    /// and that `None` must be ignored — propagating it would force the
    /// whole `root_xref` call to fail and fall back to a heuristic scan
    /// that loses the latest revision's overrides.
    #[test]
    fn shared_xref_stm_across_revisions_loads_latest_classic_override() {
        let mut pdf: Vec<u8> = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.7\n");

        // --- Revision 1: original catalog + a shared XRefStm. ---
        let original_offset = pdf.len();
        pdf.extend_from_slice(
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Marker /Original >>\nendobj\n",
        );

        let obj2_offset = pdf.len();
        pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");

        // The shared XRefStm — re-asserts obj 1 at its rev-1 offset. Rev 2's
        // trailer will reference this same offset.
        let xref_stm_offset = pdf.len();
        let mut stream_data: Vec<u8> = Vec::with_capacity(6);
        stream_data.push(0x01);
        stream_data.extend_from_slice(&(original_offset as u32).to_be_bytes());
        stream_data.push(0x00);
        let stream_len = stream_data.len();
        pdf.extend_from_slice(
            format!(
                "3 0 obj\n<< /Type /XRef /Size 4 /W [ 1 4 1 ] /Index [ 1 1 ] \
                 /Length {stream_len} /Root 1 0 R >>\nstream\n"
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&stream_data);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");

        let xref_a_offset = pdf.len();
        pdf.extend_from_slice(b"xref\n0 4\n");
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        pdf.extend_from_slice(format!("{original_offset:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{obj2_offset:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{xref_stm_offset:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size 4 /Root 1 0 R /XRefStm {xref_stm_offset} >>\n\
                 startxref\n{xref_a_offset}\n%%EOF\n"
            )
            .as_bytes(),
        );

        // --- Revision 2: incremental update with a new catalog body. ---
        let updated_offset = pdf.len();
        pdf.extend_from_slice(
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Marker /Updated >>\nendobj\n",
        );

        let xref_b_offset = pdf.len();
        pdf.extend_from_slice(b"xref\n1 1\n");
        pdf.extend_from_slice(format!("{updated_offset:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size 4 /Root 1 0 R /Prev {xref_a_offset} \
                 /XRefStm {xref_stm_offset} >>\n\
                 startxref\n{xref_b_offset}\n%%EOF"
            )
            .as_bytes(),
        );

        let pdf_doc = Pdf::new(pdf).expect("two-revision hybrid loads");
        let entry = pdf_doc
            .xref()
            .entry(ObjectIdentifier::new(1, 0))
            .expect("object 1 has an xref entry");
        match entry {
            EntryType::Normal { offset } => assert_eq!(
                offset, updated_offset,
                "rev 2's classic xref override must win over rev 1's classic + the shared XRefStm \
                 (got offset {offset}, expected {updated_offset})"
            ),
            other => panic!("expected Normal entry, got {other:?}"),
        }
    }

    /// In a hybrid file the classic xref table is the legacy-reader fallback
    /// and the `/XRefStm` is authoritative for PDF 1.5+ readers
    /// (PDF 32000-1 § 7.5.8.4). When both reference the same object number,
    /// the stream's entry must win.
    #[test]
    fn hybrid_xref_stm_overrides_classic_entries() {
        let mut pdf: Vec<u8> = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.7\n");

        // Object 1, "legacy" body, referenced by the classic xref table.
        let legacy_offset = pdf.len();
        pdf.extend_from_slice(
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /TestMarker /Legacy >>\nendobj\n",
        );

        let obj2_offset = pdf.len();
        pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");

        // Object 1, "new" body, referenced by the XRefStm.
        let new_offset = pdf.len();
        pdf.extend_from_slice(
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /TestMarker /New >>\nendobj\n",
        );

        // Object 4 — the XRefStm — overrides only object 1.
        let xref_stm_offset = pdf.len();
        let mut stream_data: Vec<u8> = Vec::with_capacity(6);
        stream_data.push(0x01); // in-use
        stream_data.extend_from_slice(&(new_offset as u32).to_be_bytes());
        stream_data.push(0x00); // generation
        let stream_len = stream_data.len();
        pdf.extend_from_slice(
            format!(
                "4 0 obj\n<< /Type /XRef /Size 5 /W [ 1 4 1 ] /Index [ 1 1 ] \
                 /Length {stream_len} /Root 1 0 R >>\nstream\n"
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&stream_data);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");

        // Classic xref table — points object 1 at the LEGACY body, which is
        // the entry the spec expects pre-1.5 readers to follow.
        let xref_offset = pdf.len();
        pdf.extend_from_slice(b"xref\n0 5\n");
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        pdf.extend_from_slice(format!("{legacy_offset:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{obj2_offset:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(b"0000000000 00001 f \n");
        pdf.extend_from_slice(format!("{xref_stm_offset:010} 00000 n \n").as_bytes());

        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size 5 /Root 1 0 R /XRefStm {xref_stm_offset} >>\n\
                 startxref\n{xref_offset}\n%%EOF"
            )
            .as_bytes(),
        );

        let pdf_doc = Pdf::new(pdf).expect("hybrid pdf loads");
        let entry = pdf_doc
            .xref()
            .entry(ObjectIdentifier::new(1, 0))
            .expect("object 1 has an xref entry");
        match entry {
            EntryType::Normal { offset } => assert_eq!(
                offset, new_offset,
                "XRefStm entry must override classic xref for the same revision \
                 (got offset {offset}, expected {new_offset})"
            ),
            other => panic!("expected Normal entry, got {other:?}"),
        }
    }
}
