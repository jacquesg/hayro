//! Name trees.
//!
//! ISO 32000-1 §7.9.6 defines name trees: an ordered map from byte-string
//! keys to arbitrary PDF objects, used for `/Dests`, `/EmbeddedFiles`,
//! `/JavaScript`, `/AP`, and application-defined entries under `/Names`.
//! Every consumer used to re-implement the `/Kids` + `/Names` recursion;
//! this module provides it once.

use crate::object::{Array, Dict, Object};
use alloc::vec::Vec;

/// A PDF name tree.
///
/// Wraps a dict that is either a name-tree root, an intermediate node,
/// or a leaf. Use [`NameTree::iter`] for a depth-first enumeration of
/// every `(key, value)` pair in the tree, or [`NameTree::get`] for a
/// targeted lookup that uses each node's `/Limits` to skip subtrees
/// that cannot contain the key.
///
/// # Keys
///
/// Keys are yielded as owned `Vec<u8>` rather than borrowed slices
/// because a hex or escape-expanded string key's decoded bytes are not
/// contiguous in the PDF source. Callers who need the pre-decode bytes
/// can re-fetch the key as a [`crate::object::String`] and call
/// [`crate::object::String::source`].
#[derive(Clone, Debug)]
pub struct NameTree<'a> {
    root: Dict<'a>,
}

impl<'a> NameTree<'a> {
    /// Wrap `dict` as a name-tree root. Returns `None` for dicts that
    /// are not valid name-tree nodes (i.e. have neither `/Names` nor
    /// `/Kids`).
    pub fn new(dict: Dict<'a>) -> Option<Self> {
        // A valid node has /Names (an array of alternating key/value) OR
        // /Kids (an array of indirect references to child nodes). Having
        // neither — or having /Names as a dict (e.g. the catalog's own
        // `/Names` entry, which is a lookup-root dict, not a tree node) —
        // means this isn't a name-tree node.
        let has_names = dict.get::<Array<'a>>(b"Names").is_some();
        let has_kids = dict.get::<Array<'a>>(b"Kids").is_some();
        if has_names || has_kids {
            Some(Self { root: dict })
        } else {
            None
        }
    }

    /// Iterate over every `(key, value)` pair in the tree, traversing
    /// `/Kids` recursively.
    ///
    /// The walk is depth-first, so the iteration order matches the
    /// tree's physical layout.
    pub fn iter(&self) -> alloc::vec::IntoIter<(Vec<u8>, Object<'a>)> {
        let mut results: Vec<(Vec<u8>, Object<'a>)> = Vec::new();
        collect_pairs(&self.root, &mut results);
        results.into_iter()
    }

    /// Look up a value by key.
    ///
    /// Uses each node's `/Limits` to prune subtrees that cannot contain
    /// the key. Returns `None` if the key is absent.
    pub fn get(&self, key: &[u8]) -> Option<Object<'a>> {
        find_in_node(&self.root, key)
    }
}

fn collect_pairs<'a>(node: &Dict<'a>, out: &mut Vec<(Vec<u8>, Object<'a>)>) {
    if let Some(names) = node.get::<Array<'a>>(b"Names") {
        let mut iter = names.flex_iter();
        loop {
            let Some(key) = iter.next::<crate::object::String<'a>>() else {
                break;
            };
            let Some(value) = iter.next::<Object<'a>>() else {
                break;
            };
            out.push((key.as_bytes().to_vec(), value));
        }
    }

    if let Some(kids) = node.get::<Array<'a>>(b"Kids") {
        for kid in kids.iter::<Dict<'a>>() {
            collect_pairs(&kid, out);
        }
    }
}

fn find_in_node<'a>(node: &Dict<'a>, key: &[u8]) -> Option<Object<'a>> {
    // `/Limits` optimisation: skip any subtree whose range doesn't
    // include the query key. The root is permitted to omit `/Limits`
    // (and must, per the spec), so absence is fine.
    if let Some(limits) = node.get::<Array<'a>>(b"Limits") {
        let mut iter = limits.flex_iter();
        let lo = iter.next::<crate::object::String<'a>>()?;
        let hi = iter.next::<crate::object::String<'a>>()?;
        if key < lo.as_bytes() || key > hi.as_bytes() {
            return None;
        }
    }

    if let Some(names) = node.get::<Array<'a>>(b"Names") {
        let mut iter = names.flex_iter();
        loop {
            let Some(entry_key) = iter.next::<crate::object::String<'a>>() else {
                break;
            };
            let Some(entry_value) = iter.next::<Object<'a>>() else {
                break;
            };
            if entry_key.as_bytes() == key {
                return Some(entry_value);
            }
        }
    }

    if let Some(kids) = node.get::<Array<'a>>(b"Kids") {
        for kid in kids.iter::<Dict<'a>>() {
            if let Some(v) = find_in_node(&kid, key) {
                return Some(v);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Pdf;
    use crate::object::Object;
    use alloc::format;

    /// Build a PDF whose /Root/Names/Dests is a flat name tree with
    /// three string-keyed entries.
    fn build_pdf_with_flat_name_tree() -> Vec<u8> {
        let mut pdf: Vec<u8> = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.7\n");

        let off1 = pdf.len();
        pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Names 4 0 R >>\nendobj\n");
        let off2 = pdf.len();
        pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        // Dests tree (flat).
        let off3 = pdf.len();
        pdf.extend_from_slice(
            b"3 0 obj\n<< /Names [(alpha) 10 0 R (beta) 11 0 R (gamma) 12 0 R] >>\nendobj\n",
        );
        // /Names dict referencing the dests tree.
        let off4 = pdf.len();
        pdf.extend_from_slice(b"4 0 obj\n<< /Dests 3 0 R >>\nendobj\n");
        // Dummy targets.
        let off10 = pdf.len();
        pdf.extend_from_slice(b"10 0 obj\n<< /Value 1 >>\nendobj\n");
        let off11 = pdf.len();
        pdf.extend_from_slice(b"11 0 obj\n<< /Value 2 >>\nendobj\n");
        let off12 = pdf.len();
        pdf.extend_from_slice(b"12 0 obj\n<< /Value 3 >>\nendobj\n");

        let xref_pos = pdf.len();
        pdf.extend_from_slice(b"xref\n0 13\n0000000000 65535 f \n");
        pdf.extend_from_slice(format!("{off1:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off2:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off3:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off4:010} 00000 n \n").as_bytes());
        // Free slots for 5..9 to keep object numbers as used.
        for _ in 5..10 {
            pdf.extend_from_slice(b"0000000000 00001 f \n");
        }
        pdf.extend_from_slice(format!("{off10:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off11:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off12:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(b"trailer\n<< /Size 13 /Root 1 0 R >>\n");
        pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF").as_bytes());
        pdf
    }

    /// Build a PDF with a name tree using `/Kids` and `/Limits`:
    /// root has two children, each a leaf with one entry.
    fn build_pdf_with_nested_name_tree() -> Vec<u8> {
        let mut pdf: Vec<u8> = Vec::new();
        pdf.extend_from_slice(b"%PDF-1.7\n");
        let off1 = pdf.len();
        pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Names 3 0 R >>\nendobj\n");
        let off2 = pdf.len();
        pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        let off3 = pdf.len();
        pdf.extend_from_slice(b"3 0 obj\n<< /Dests 4 0 R >>\nendobj\n");
        // Root with Kids.
        let off4 = pdf.len();
        pdf.extend_from_slice(b"4 0 obj\n<< /Kids [5 0 R 6 0 R] >>\nendobj\n");
        // Leaf 1: covers "alpha"..."alpha".
        let off5 = pdf.len();
        pdf.extend_from_slice(
            b"5 0 obj\n<< /Limits [(alpha) (alpha)] /Names [(alpha) 10 0 R] >>\nendobj\n",
        );
        // Leaf 2: covers "cats"..."dogs".
        let off6 = pdf.len();
        pdf.extend_from_slice(
            b"6 0 obj\n<< /Limits [(cats) (dogs)] /Names [(cats) 11 0 R (dogs) 12 0 R] >>\nendobj\n",
        );
        let off10 = pdf.len();
        pdf.extend_from_slice(b"10 0 obj\n<< /Value 1 >>\nendobj\n");
        let off11 = pdf.len();
        pdf.extend_from_slice(b"11 0 obj\n<< /Value 2 >>\nendobj\n");
        let off12 = pdf.len();
        pdf.extend_from_slice(b"12 0 obj\n<< /Value 3 >>\nendobj\n");

        let xref_pos = pdf.len();
        pdf.extend_from_slice(b"xref\n0 13\n0000000000 65535 f \n");
        for off in [off1, off2, off3, off4, off5, off6] {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        for _ in 7..10 {
            pdf.extend_from_slice(b"0000000000 00001 f \n");
        }
        pdf.extend_from_slice(format!("{off10:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off11:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(format!("{off12:010} 00000 n \n").as_bytes());
        pdf.extend_from_slice(b"trailer\n<< /Size 13 /Root 1 0 R >>\n");
        pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF").as_bytes());
        pdf
    }

    fn dests_root<'a>(pdf: &'a Pdf) -> NameTree<'a> {
        let root_dict = pdf
            .xref()
            .get::<Dict<'a>>(pdf.xref().root_id())
            .expect("catalog");
        let names = root_dict.get::<Dict<'a>>(b"Names").expect("Names entry");
        let dests = names.get::<Dict<'a>>(b"Dests").expect("Dests entry");
        NameTree::new(dests).expect("valid name tree")
    }

    #[test]
    fn flat_name_tree_iter_yields_three_pairs() {
        let bytes = build_pdf_with_flat_name_tree();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = dests_root(&pdf);
        let collected: Vec<(Vec<u8>, Object<'_>)> = tree.iter().collect();
        assert_eq!(collected.len(), 3);
        let keys: Vec<_> = collected.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(
            keys,
            alloc::vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]
        );
    }

    #[test]
    fn flat_name_tree_get_hits_and_misses() {
        let bytes = build_pdf_with_flat_name_tree();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = dests_root(&pdf);
        assert!(tree.get(b"alpha").is_some());
        assert!(tree.get(b"beta").is_some());
        assert!(tree.get(b"gamma").is_some());
        assert!(tree.get(b"missing").is_none());
    }

    #[test]
    fn nested_name_tree_iter_flattens_all_entries() {
        let bytes = build_pdf_with_nested_name_tree();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = dests_root(&pdf);
        let collected: Vec<(Vec<u8>, Object<'_>)> = tree.iter().collect();
        assert_eq!(collected.len(), 3);
    }

    #[test]
    fn nested_name_tree_get_prunes_via_limits() {
        let bytes = build_pdf_with_nested_name_tree();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = dests_root(&pdf);
        assert!(tree.get(b"alpha").is_some());
        assert!(tree.get(b"cats").is_some());
        assert!(tree.get(b"dogs").is_some());
        // Key outside all /Limits ranges — pruned by both children.
        assert!(tree.get(b"zzz").is_none());
        // Key in the hole between the two subtrees.
        assert!(tree.get(b"bravo").is_none());
    }

    #[test]
    fn new_rejects_dict_without_names_or_kids() {
        let bytes = build_pdf_with_flat_name_tree();
        let pdf = Pdf::new(bytes).expect("pdf loads");
        // The catalog itself has neither /Names (as a leaf) nor /Kids.
        let catalog = pdf
            .xref()
            .get::<Dict<'_>>(pdf.xref().root_id())
            .expect("catalog");
        assert!(NameTree::new(catalog).is_none());
    }
}
