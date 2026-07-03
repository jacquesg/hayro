//! Name trees.
//!
//! ISO 32000-2:2020 §7.9.6 (pp. 134–137; Table 36 on p. 135) defines name
//! trees: an ordered map from byte-string keys to arbitrary PDF objects,
//! reached through the document's `/Names` dictionary and used for
//! categories such as `/Dests`, `/EmbeddedFiles`, and `/JavaScript` (7.7.4,
//! Table 32).
//!
//! A tree is a graph of node dictionaries: a root holding either `/Kids` or
//! `/Names`, intermediate nodes holding `/Kids` + `/Limits`, and leaf nodes
//! holding `/Names` + `/Limits`. [`NameTree`] walks that graph:
//! [`NameTree::iter`] lazily enumerates every pair in key order, and
//! [`NameTree::get`] resolves a single key, using each node's `/Limits`
//! range to skip subtrees that cannot contain it.
//!
//! Both operations are hardened against malformed trees that adversarial
//! PDFs use to force non-termination, super-linear work, or a stack
//! overflow: `/Kids` cycles and shared (DAG) subtrees are each entered at
//! most once, a shared budget caps the total work (nodes entered plus
//! `/Kids` and `/Names` entries examined — so a `/Kids` or `/Names` array
//! shared by many nodes cannot be re-scanned once per node for free),
//! traversal depth is
//! capped so deeply nested `/Kids` (inline or indirect) cannot overflow the
//! native stack — `iter` walks on a heap stack, `get`'s recursion is
//! depth-bounded (see `MAX_NODE_DEPTH`) — and dangling or mistyped `/Names`
//! and `/Kids` entries are skipped rather than aborting the walk.

use crate::object::array::ArrayIter;
use crate::object::{Array, Dict, MaybeRef, Object, ObjectIdentifier, String};
use crate::reader::ReaderContext;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::cmp::Ordering;

/// Upper bound on a single traversal's total work: one unit per tree node
/// entered *and* one per `/Kids` or `/Names` entry examined (see
/// [`KidsCursor::next_child`] and [`LeafCursor::next_pair`]).
///
/// The resolved-node `visited` set already collapses shared subtrees and
/// `/Kids` cycles to one visit each, and the reader caps indirect-object
/// nesting depth, so a well-behaved tree never approaches this. It is
/// belt-and-braces against a tree made of very many *distinct* nodes (e.g.
/// a root with a huge flat `/Kids` fan-out) and against a `/Kids` or `/Names`
/// array shared by many nodes and re-scanned once per node — where neither
/// depth nor the visited set (which dedupes node dicts, not the arrays they
/// point at) bounds the work. Counting entries examined, not just nodes
/// entered, keeps those scans O(this cap) rather than O(nodes × array
/// length). ISO 32000-2:2020 §7.9.6 places no bound on node or entry count,
/// so this is a processing limit, not a spec limit.
const MAX_NODE_VISITS: usize = 100_000;

/// Upper bound on `find_in_node`'s recursion depth — the length of the
/// root-to-node path [`NameTree::get`] will descend before giving up.
///
/// `get` searches recursively, one `find_in_node` frame per tree level, so a
/// tree of thousands of nested `/Kids` nodes would recurse to a native stack
/// overflow (an unrecoverable `SIGABRT`, no unwind). The reader's own
/// indirect-reference ceiling (`MAX_INDIRECT_OBJECT_DEPTH = 256`) already
/// bounds a chain of *indirect* `/Kids` references, but a child dict written
/// *inline* in `/Kids` does not grow that chain (see `KidsCursor::next_child`),
/// so inline nesting is unbounded by it. This cap bounds the recursion
/// whatever the encoding, set to the same 256 so inline and indirect nesting
/// are treated identically. `MAX_NODE_VISITS` does not help: it bounds fan-out
/// breadth, not depth, and the stack overflows long before that budget is
/// spent. [`NameTreeIter`] needs no such cap — it walks on an explicit heap
/// stack. ISO 32000-2:2020 §7.9.6 places no bound on tree depth, so this is a
/// processing limit, not a spec limit.
const MAX_NODE_DEPTH: usize = 256;

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
    /// `/Kids` depth-first.
    ///
    /// The returned iterator walks lazily, yielding pairs on demand. For a
    /// well-formed tree — leaves sorted, and `/Limits` ordered per §7.9.6 —
    /// the yield order is ascending key order.
    ///
    /// Memory has two independent parts. The traversal stack holds only the
    /// current root-to-node path, so it is O(tree depth). Separately, a
    /// `visited` set of node ids — the guard that makes each shared subtree
    /// and `/Kids` cycle enter at most once — gains at most one id per
    /// `/Kids` entry the walk examines. Examining an entry spends a unit of
    /// the traversal budget before the insert (see `KidsCursor::next_child`),
    /// so the set is bounded by `MAX_NODE_VISITS`, keeping the walk
    /// memory-safe.
    pub fn iter(&self) -> NameTreeIter<'a> {
        NameTreeIter::new(&self.root)
    }

    /// Look up a value by key.
    ///
    /// Uses each node's `/Limits` to prune subtrees that cannot contain
    /// the key, and — because a leaf's keys are sorted ascending
    /// (§7.9.6) — stops scanning a leaf once it passes the key. Returns
    /// `None` if the key is absent.
    ///
    /// For a spec-conformant tree — leaves sorted ascending, and each
    /// `/Limits` honestly bounding its subtree (§7.9.6) — membership agrees
    /// with [`NameTree::iter`], within the shared processing caps: `get(k)`
    /// is `Some` iff `iter()` yields `k`. Three things break the agreement.
    /// A malformed tree: `get` prunes on `/Limits` and stops a leaf scan
    /// once keys pass the target while `iter` reads neither, so an unsorted
    /// leaf, or a non-root `/Limits` that does not bound its contents, can
    /// make `get` miss a key that `iter` still yields. A pathologically deep
    /// tree: `get`'s recursion stops after `MAX_NODE_DEPTH` levels of `/Kids`
    /// nesting and returns `None`, while `iter` — bounded by total work, not
    /// depth — keeps descending, so it yields a key `get` misses. And the
    /// mirror case, a conformant tree exceeding `MAX_NODE_VISITS` (which both
    /// share): `iter`'s depth-first walk is cut off when the budget is spent,
    /// so a key far to the right in key order is never reached, while `get`'s
    /// `/Limits`-guided descent still prunes straight to its leaf in O(depth)
    /// visits and returns `Some`. A root `/Limits` (not permitted in root
    /// nodes per §7.9.6, Table 36; p. 135) and a `/Limits` with fewer than
    /// two strings are each treated as absent and never prune.
    pub fn get(&self, key: &[u8]) -> Option<Object<'a>> {
        self.get_bounded(key, MAX_NODE_VISITS).0
    }

    /// The shared body of [`NameTree::get`], with the work cap made
    /// injectable and the number of distinct nodes resolved (the `visited`-set
    /// size) reported alongside the result.
    ///
    /// [`NameTree::get`] calls this with [`MAX_NODE_VISITS`] and drops the
    /// count. Tests call it with a small cap to pin that the descent in
    /// [`find_in_node`] stops once the budget is spent — whether on entering
    /// many distinct siblings, on examining a shared `/Kids` array's
    /// already-visited refs, or on scanning a shared `/Names` array's entries —
    /// so the count stays within the cap rather than growing to the full
    /// `/Kids` fan-out or the per-leaf `/Names` re-scan. ISO 32000-2:2020
    /// §7.9.6 (pp. 134–137) sets no bound on node or entry count, so the cap is
    /// a processing limit, not a spec limit.
    fn get_bounded(&self, key: &[u8], max_visits: usize) -> (Option<Object<'a>>, usize) {
        let mut visited = BTreeSet::new();
        // Seed with the root's id so a descendant that references the root
        // is treated as an already-visited subtree.
        if let Some(id) = self.root.obj_id() {
            visited.insert(id);
        }
        let mut budget = max_visits;
        // The root is passed with `is_root = true` so its `/Limits` (spec-
        // forbidden on the root) is never used to prune — see `find_in_node`.
        let found = find_in_node(&self.root, key, true, 0, &mut visited, &mut budget);
        (found, visited.len())
    }
}

/// Depth-first cursor over a [`NameTree`]'s `(key, value)` pairs.
///
/// Returned by [`NameTree::iter`]. The walk is bounded even for adversarial
/// trees: a `visited` set of resolved node ids collapses shared subtrees
/// and `/Kids` cycles to a single visit, and `budget` caps the total work —
/// nodes entered plus `/Kids` and `/Names` entries examined. ISO 32000-2:2020
/// §7.9.6.
pub struct NameTreeIter<'a> {
    /// The active root-to-node path. Each frame drains its node's `/Names`
    /// pairs, then descends its `/Kids`.
    stack: Vec<Frame<'a>>,
    /// Resolved ids of nodes already entered (N1: bounds shared-subtree /
    /// cyclic `/Kids` graphs).
    visited: BTreeSet<ObjectIdentifier>,
    /// Remaining work budget: one unit per node entered and per `/Kids` or
    /// `/Names` entry examined (belt-and-braces total-work cap).
    budget: usize,
}

impl<'a> NameTreeIter<'a> {
    fn new(root: &Dict<'a>) -> Self {
        let mut visited = BTreeSet::new();
        if let Some(id) = root.obj_id() {
            visited.insert(id);
        }
        let mut budget = MAX_NODE_VISITS;
        let mut stack = Vec::new();
        if budget > 0 {
            budget -= 1;
            stack.push(Frame::new(root));
        }
        Self {
            stack,
            visited,
            budget,
        }
    }
}

impl<'a> Iterator for NameTreeIter<'a> {
    type Item = (Vec<u8>, Object<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let Self {
            stack,
            visited,
            budget,
        } = self;
        loop {
            let frame = stack.last_mut()?;

            // Drain this node's leaf pairs before descending, matching the
            // yield order of a well-formed tree (leaves before kids). The drain
            // spends `budget` per `/Names` entry (see `LeafCursor::next_pair`),
            // so a shared indirect `/Names` array re-drained once per leaf is
            // capped at `MAX_NODE_VISITS` total entries rather than K × array
            // length — the same total-work cap the `/Kids` descent honours.
            if let Some(leaf) = frame.leaf.as_mut() {
                while let Some((key, value_token)) = leaf.next_pair(budget) {
                    // N3: a pair whose value cannot be resolved is skipped,
                    // not fatal — the rest of the leaf still yields.
                    if let Some(value) = value_token.resolve(&leaf.ctx) {
                        return Some((key.as_bytes().to_vec(), value));
                    }
                }
            }

            // Leaf exhausted: descend into the next child, or pop.
            let child = match frame.kids.as_mut() {
                Some(kids) => kids.next_child(visited, budget),
                None => None,
            };
            match child {
                Some(child) if *budget > 0 => {
                    *budget -= 1;
                    stack.push(Frame::new(&child));
                }
                // No more children, or the node-visit budget is spent: this
                // node is done. Popping unwinds the whole walk once spent.
                _ => {
                    stack.pop();
                }
            }
        }
    }
}

/// One node on the traversal stack: a cursor over its `/Names` leaf pairs
/// and a cursor over its `/Kids` children. Both are `None` when the node
/// lacks that entry.
struct Frame<'a> {
    leaf: Option<LeafCursor<'a>>,
    kids: Option<KidsCursor<'a>>,
}

impl<'a> Frame<'a> {
    fn new(node: &Dict<'a>) -> Self {
        Self {
            leaf: node.get::<Array<'a>>(b"Names").map(|a| LeafCursor::new(&a)),
            kids: node.get::<Array<'a>>(b"Kids").map(|a| KidsCursor::new(&a)),
        }
    }
}

/// Cursor over a leaf node's `/Names` array — the flat `[key1 value1 key2
/// value2 …]` list of §7.9.6, Table 36.
///
/// Tokens are consumed two at a time, so a dangling or mistyped entry never
/// desynchronises key/value pairing (N3): the raw iterator reports genuine
/// end-of-array as `None`, which is distinct from a token that fails to
/// resolve.
struct LeafCursor<'a> {
    tokens: ArrayIter<'a>,
    ctx: ReaderContext<'a>,
}

impl<'a> LeafCursor<'a> {
    fn new(names: &Array<'a>) -> Self {
        Self {
            tokens: names.raw_iter(),
            ctx: names.ctx().clone(),
        }
    }

    /// The next `(key, value-token)` pair, or `None` at end-of-array, a
    /// trailing key with no value, or once `budget` is spent. Pairs whose key
    /// does not resolve to a string are skipped. The value is returned
    /// unresolved so callers can resolve it lazily (only on a hit, in `get`).
    ///
    /// `budget` bounds the `/Names` entries *examined*, mirroring
    /// [`KidsCursor::next_child`]: one unit is spent per entry inspected —
    /// including a mistyped/dangling entry skipped below — and the scan stops
    /// once it reaches zero. Without this a leaf's `/Names` array is scanned
    /// for free, so one indirect `/Names` array shared by K leaves and
    /// re-scanned once per leaf would be O(K × array length) work that the
    /// node-entry cap alone never bounded. §7.9.6 places no bound on entry
    /// count; see [`MAX_NODE_VISITS`].
    fn next_pair(&mut self, budget: &mut usize) -> Option<(String<'a>, MaybeRef<Object<'a>>)> {
        loop {
            // Budget every entry examined, not just each pair returned, so a
            // shared /Names array cannot be re-scanned once per leaf for free.
            if *budget == 0 {
                return None;
            }
            let key_token = self.tokens.next()?;
            *budget -= 1;
            // Odd-length /Names: a trailing key with no value — drop it.
            let value_token = self.tokens.next()?;
            let Some(key) = key_token.resolve(&self.ctx).and_then(Object::into_string) else {
                // Mistyped/dangling key: skip this pair, stay aligned.
                continue;
            };
            return Some((key, value_token));
        }
    }
}

/// Cursor over an intermediate/root node's `/Kids` array, resolving each
/// child node dictionary.
///
/// Skips children that are already-visited (shared or cyclic) subtrees and
/// children that fail to resolve to a dict, rather than aborting.
struct KidsCursor<'a> {
    tokens: ArrayIter<'a>,
    ctx: ReaderContext<'a>,
}

impl<'a> KidsCursor<'a> {
    fn new(kids: &Array<'a>) -> Self {
        Self {
            tokens: kids.raw_iter(),
            ctx: kids.ctx().clone(),
        }
    }

    /// The next child node, or `None` at end-of-array or once `budget` is
    /// spent. `visited` records the ids of nodes already entered; a `/Kids`
    /// reference to one of them (a shared subtree or a cycle) is skipped
    /// (N1). Dangling or non-dict children are skipped too (N3).
    ///
    /// `budget` bounds the `/Kids` entries *examined*, not just the children
    /// returned: one unit is spent per entry inspected, including entries
    /// skipped as already-visited or unresolvable, and the scan stops once it
    /// reaches zero. Without this a single call could scan an unbounded run of
    /// already-visited refs — and one indirect `/Kids` array shared by K nodes
    /// and re-scanned once per node would be O(K × array length) work that the
    /// node-entry cap alone never bounded. §7.9.6 places no bound on node
    /// count; see [`MAX_NODE_VISITS`].
    fn next_child(
        &mut self,
        visited: &mut BTreeSet<ObjectIdentifier>,
        budget: &mut usize,
    ) -> Option<Dict<'a>> {
        loop {
            // Budget every entry examined, not just each child entered, so the
            // scan cannot outrun the traversal's total-work cap.
            if *budget == 0 {
                return None;
            }
            let token = self.tokens.next()?;
            *budget -= 1;
            let child = match token {
                MaybeRef::Ref(r) => {
                    let id = ObjectIdentifier::from(r);
                    if !visited.insert(id) {
                        // Already entered via another path (DAG) or a cycle.
                        continue;
                    }
                    match self.ctx.xref().get_with::<Dict<'a>>(id, &self.ctx) {
                        Some(dict) => dict,
                        None => continue,
                    }
                }
                // A direct (inline) child has no stable id to dedupe on, but
                // is unique to its slot and cannot be revisited or form a
                // reference cycle, so it is safe to descend as-is.
                MaybeRef::NotRef(obj) => match obj.into_dict() {
                    Some(dict) => dict,
                    None => continue,
                },
            };
            return Some(child);
        }
    }
}

fn find_in_node<'a>(
    node: &Dict<'a>,
    key: &[u8],
    is_root: bool,
    depth: usize,
    visited: &mut BTreeSet<ObjectIdentifier>,
    budget: &mut usize,
) -> Option<Object<'a>> {
    // Bound recursion depth independently of `budget` (which caps total nodes,
    // i.e. fan-out breadth) and of the reader's indirect-reference ceiling
    // (which does not count inline `/Kids` children — see
    // `KidsCursor::next_child`): a chain of inline-nested nodes would otherwise
    // recurse to a native stack overflow. See `MAX_NODE_DEPTH`. `iter` needs no
    // such guard because it walks on an explicit heap stack.
    if *budget == 0 || depth >= MAX_NODE_DEPTH {
        return None;
    }
    *budget -= 1;

    // `/Limits` prune: skip a subtree whose range excludes the key. §7.9.6,
    // Table 36 (p. 135) forbids `/Limits` in the root ("not permitted in root
    // nodes"), and `iter` never reads it, so a `/Limits` on the root is
    // ignored — otherwise a spec-violating root bearing one could prune keys
    // that `iter` still yields, breaking their agreement. A malformed
    // `/Limits` (fewer than two strings) is likewise treated as absent so
    // `get` does not prune a node that `iter` would still yield (N7).
    if !is_root
        && let Some((lo, hi)) = read_limits(node)
        && (key < lo.as_bytes() || key > hi.as_bytes())
    {
        return None;
    }

    // Leaf: a node's keys are sorted ascending (§7.9.6), so once a key
    // strictly exceeds the target the target cannot appear later (N6). The
    // scan shares `budget` with the rest of the traversal — one unit per
    // `/Names` entry examined — so a leaf whose `/Names` is a shared indirect
    // array re-scanned once per sibling cannot outrun the total-work cap,
    // matching the `/Kids` scan below and `NameTreeIter::next`.
    if let Some(names) = node.get::<Array<'a>>(b"Names") {
        let mut leaf = LeafCursor::new(&names);
        while let Some((entry_key, value_token)) = leaf.next_pair(budget) {
            match entry_key.as_bytes().cmp(key) {
                Ordering::Equal => return value_token.resolve(&leaf.ctx),
                Ordering::Greater => break,
                Ordering::Less => {}
            }
        }
    }

    // Intermediate: descend, sharing the `visited`/`budget` guards with the
    // leaf search so a cyclic or shared `/Kids` graph terminates. Both the
    // loop gate and `next_child` honour `budget`: `next_child` spends one unit
    // per `/Kids` entry it examines and returns `None` once the budget is
    // zero, so neither a huge flat fan-out of distinct children nor a long run
    // of already-visited/dangling refs — including one shared `/Kids` array
    // re-scanned once per node — costs more than O(MAX_NODE_VISITS) total. This
    // matches `NameTreeIter::next`, which stops descending on the same budget.
    // §7.9.6 sets no node-count bound, so this is exactly the processing limit
    // MAX_NODE_VISITS documents.
    if let Some(kids) = node.get::<Array<'a>>(b"Kids") {
        let mut cursor = KidsCursor::new(&kids);
        while *budget > 0 {
            let Some(child) = cursor.next_child(visited, budget) else {
                break;
            };
            // Children are intermediate or leaf nodes, where `/Limits` is
            // required and honoured — hence `is_root = false`.
            if let Some(v) = find_in_node(&child, key, false, depth + 1, visited, budget) {
                return Some(v);
            }
        }
    }

    None
}

/// Read a node's `/Limits` as its `(least, greatest)` key bounds, or `None`
/// if `/Limits` is absent or malformed (fewer than two strings).
fn read_limits<'a>(node: &Dict<'a>) -> Option<(String<'a>, String<'a>)> {
    let limits = node.get::<Array<'a>>(b"Limits")?;
    let mut iter = limits.flex_iter();
    let lo = iter.next::<String<'a>>()?;
    let hi = iter.next::<String<'a>>()?;
    Some((lo, hi))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Pdf;
    use crate::object::Object;
    use alloc::collections::BTreeMap;
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

    fn push_obj(pdf: &mut Vec<u8>, offsets: &mut BTreeMap<u32, usize>, n: u32, body: &[u8]) {
        offsets.insert(n, pdf.len());
        pdf.extend_from_slice(format!("{n} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }

    /// Assemble a syntactically valid single-revision PDF from a minimal
    /// catalog (object 1) and empty page tree (object 2) plus the given
    /// `(object number, body)` objects (numbered from 3 up). An object
    /// number that is referenced but never listed here lands in a gap and
    /// resolves to nothing — i.e. a dangling reference, used to test
    /// resilience. `/Root` is object 1; name-tree roots are fetched by
    /// their own object number via [`tree_from`].
    fn build_pdf(objects: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut pdf: Vec<u8> = Vec::new();
        let mut offsets: BTreeMap<u32, usize> = BTreeMap::new();
        pdf.extend_from_slice(b"%PDF-1.7\n");

        push_obj(
            &mut pdf,
            &mut offsets,
            1,
            b"<< /Type /Catalog /Pages 2 0 R >>",
        );
        push_obj(
            &mut pdf,
            &mut offsets,
            2,
            b"<< /Type /Pages /Kids [] /Count 0 >>",
        );
        for (n, body) in objects {
            push_obj(&mut pdf, &mut offsets, *n, body);
        }

        let count = offsets.keys().copied().max().unwrap_or(2) + 1;
        let xref_pos = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {count}\n").as_bytes());
        for i in 0..count {
            match (i, offsets.get(&i)) {
                (0, _) => pdf.extend_from_slice(b"0000000000 65535 f \n"),
                (_, Some(off)) => {
                    pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
                }
                // Gap: a free slot, so a reference to `i` resolves to nothing.
                (_, None) => pdf.extend_from_slice(b"0000000000 00000 f \n"),
            }
        }
        pdf.extend_from_slice(format!("trailer\n<< /Size {count} /Root 1 0 R >>\n").as_bytes());
        pdf.extend_from_slice(format!("startxref\n{xref_pos}\n%%EOF").as_bytes());
        pdf
    }

    /// Fetch the object numbered `root_obj` and wrap it as a name-tree root.
    fn tree_from(pdf: &Pdf, root_obj: u32) -> NameTree<'_> {
        let dict = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(root_obj as i32, 0))
            .expect("tree root dict");
        NameTree::new(dict).expect("valid name tree")
    }

    /// Collect a tree's keys in iteration order.
    fn iter_keys(tree: &NameTree<'_>) -> Vec<Vec<u8>> {
        tree.iter().map(|(k, _)| k).collect()
    }

    /// Build a PDF of `count` indirect objects (numbered 3..3+count), each an
    /// inline `/Kids` chain `inline_depth` levels deep whose innermost `/Kids`
    /// references the next object; object `3+count` is a leaf holding a single
    /// `(deep)` entry. `find_in_node`'s recursion therefore descends
    /// `count * inline_depth` levels, while each object's own inline nesting
    /// stays well under the parser's nesting ceiling. The inline children never
    /// grow the reader's indirect-reference parent-chain, so only the name
    /// tree's own `MAX_NODE_DEPTH` cap bounds `get` here.
    fn build_inline_kids_chain(count: u32, inline_depth: u32) -> Vec<u8> {
        let open = "<< /Kids [ ".repeat(inline_depth as usize);
        let close = " ] >>".repeat(inline_depth as usize);
        let mut objects: Vec<(u32, Vec<u8>)> = Vec::new();
        for i in 0..count {
            let obj = 3 + i;
            let next = obj + 1;
            objects.push((obj, format!("{open}{next} 0 R{close}").into_bytes()));
        }
        objects.push((3 + count, b"<< /Names [(deep) 1] >>".to_vec()));
        build_pdf(&objects)
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

    // §7.9.6: "/Kids ... shall be an array of indirect references to the
    // immediate children of this node." A node that lists itself is not a
    // valid child; the walk must terminate rather than recurse forever.
    #[test]
    fn cyclic_kids_self_loop_terminates() {
        let bytes = build_pdf(&alloc::vec![(3_u32, b"<< /Kids [3 0 R] >>".to_vec())]);
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = tree_from(&pdf, 3);
        // Reaching these assertions at all proves termination.
        assert_eq!(tree.iter().count(), 0);
        assert!(tree.get(b"anything").is_none());
    }

    // §7.9.6: two intermediate nodes whose /Kids reference each other. The
    // reachable leaf must still be yielded exactly once and the mutual
    // reference must not loop.
    #[test]
    fn cyclic_kids_mutual_loop_terminates() {
        let bytes = build_pdf(&alloc::vec![
            // A -> B
            (3_u32, b"<< /Kids [4 0 R] >>".to_vec()),
            // B -> leaf C, then back to A (the cycle edge).
            (4_u32, b"<< /Kids [5 0 R 3 0 R] >>".to_vec()),
            (5_u32, b"<< /Limits [(k)(k)] /Names [(k) 1] >>".to_vec()),
        ]);
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = tree_from(&pdf, 3);
        assert_eq!(iter_keys(&tree), alloc::vec![b"k".to_vec()]);
        assert!(tree.get(b"k").is_some());
        assert!(tree.get(b"missing").is_none());
    }

    // §7.9.6 places no bound on node count, but a shared subtree reached by
    // two /Kids edges at every level would fan out to 2^depth visits if the
    // walk did not dedupe. With `depth = 60` an unbounded walk (~10^18
    // visits) could never finish; the visited set keeps it O(depth).
    #[test]
    fn shared_subtree_dag_is_bounded_not_exponential() {
        const DEPTH: u32 = 60;
        let mut objects: Vec<(u32, Vec<u8>)> = Vec::new();
        for i in 0..DEPTH {
            let obj = 3 + i;
            let child = obj + 1;
            // Each level references the same child twice (the DAG edge).
            objects.push((
                obj,
                format!("<< /Kids [{child} 0 R {child} 0 R] >>").into_bytes(),
            ));
        }
        objects.push((3 + DEPTH, b"<< /Names [(deep) 1] >>".to_vec()));

        let pdf = Pdf::new(build_pdf(&objects)).expect("pdf loads");
        let tree = tree_from(&pdf, 3);
        // The single leaf is reached once, not 2^60 times.
        assert_eq!(tree.iter().count(), 1);
        assert!(tree.get(b"deep").is_some());
    }

    // A deep linear /Kids chain must terminate rather than overflow the
    // stack. The reader caps indirect-object nesting (MAX_INDIRECT_OBJECT_-
    // DEPTH = 256) and the node-visit budget caps total work, so a chain far
    // deeper than that stops with the bottom leaf simply unreachable.
    #[test]
    fn deep_kids_chain_terminates() {
        const LEN: u32 = 400;
        let mut objects: Vec<(u32, Vec<u8>)> = Vec::new();
        for i in 0..LEN {
            let obj = 3 + i;
            let child = obj + 1;
            objects.push((obj, format!("<< /Kids [{child} 0 R] >>").into_bytes()));
        }
        objects.push((3 + LEN, b"<< /Names [(bottom) 1] >>".to_vec()));

        let pdf = Pdf::new(build_pdf(&objects)).expect("pdf loads");
        let tree = tree_from(&pdf, 3);
        // Bottom leaf is past the depth ceiling, so nothing is yielded — but
        // crucially the call returns instead of hanging / overflowing.
        assert_eq!(tree.iter().count(), 0);
        assert!(tree.get(b"bottom").is_none());
    }

    // `NameTree::get` searches recursively (one `find_in_node` frame per tree
    // level), so without a depth cap a deeply nested `/Kids` chain overflows
    // the native stack — an unrecoverable SIGABRT. The reader's indirect-
    // reference ceiling does not save it: inline (direct) `/Kids` children
    // never grow that parent-chain (`KidsCursor::next_child`), so inline
    // nesting is unbounded by it, and `MAX_NODE_VISITS` bounds only fan-out
    // breadth, not depth. `MAX_NODE_DEPTH` bounds the recursion whatever the
    // encoding. `iter`, walking on a heap stack, is immune and still reaches
    // the bottom leaf; `get` returns None rather than aborting. This pins that
    // iter/get asymmetry — a case the indirect-only `deep_kids_chain_-
    // terminates` never covers. §7.9.6 places no bound on tree depth.
    #[test]
    fn deep_inline_kids_get_returns_none_iter_still_yields() {
        // count * inline_depth = 3000 levels: past MAX_NODE_DEPTH (256) and
        // past the ~2000-frame native stack limit a pre-fix `get` would blow,
        // while each object's 300-deep inline nesting parses without issue.
        const COUNT: u32 = 10;
        const INLINE_DEPTH: u32 = 300;
        let bytes = build_inline_kids_chain(COUNT, INLINE_DEPTH);
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = tree_from(&pdf, 3);

        // `iter` walks the inline nesting on the heap and reaches the leaf.
        assert_eq!(iter_keys(&tree), alloc::vec![b"deep".to_vec()]);
        // `get` recurses; the depth cap makes it return None. Reaching this
        // assertion at all — rather than SIGABRT — is the regression guard.
        assert!(tree.get(b"deep").is_none());
    }

    // §7.9.6 Table 36: /Names is `[key1 value1 key2 value2 …]`. A trailing
    // key with no value is malformed; it is dropped, not paired with the
    // next entry or treated as fatal.
    #[test]
    fn odd_length_names_drops_trailing_key() {
        let bytes = build_pdf(&alloc::vec![(3_u32, b"<< /Names [(a) 1 (b)] >>".to_vec())]);
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = tree_from(&pdf, 3);
        assert_eq!(iter_keys(&tree), alloc::vec![b"a".to_vec()]);
        assert!(tree.get(b"a").is_some());
        // "b" had no value, so it is not a member.
        assert!(tree.get(b"b").is_none());
    }

    // A /Names value that is an indirect reference to an undefined object
    // resolves to nothing. That pair is skipped without dropping the
    // entries after it (the pre-fix loop broke on the first miss).
    #[test]
    fn dangling_value_ref_is_skipped_and_later_entries_found() {
        let bytes = build_pdf(&alloc::vec![
            // Object 99 is never defined — a dangling value.
            (
                3_u32,
                b"<< /Names [(a) 10 0 R (b) 99 0 R (c) 12 0 R] >>".to_vec()
            ),
            (10_u32, b"<< /V 1 >>".to_vec()),
            (12_u32, b"<< /V 3 >>".to_vec()),
        ]);
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = tree_from(&pdf, 3);
        // "b" is skipped, but "c" — which came after it — is still found.
        assert_eq!(iter_keys(&tree), alloc::vec![b"a".to_vec(), b"c".to_vec()]);
        assert!(tree.get(b"a").is_some());
        assert!(tree.get(b"b").is_none());
        assert!(tree.get(b"c").is_some());
    }

    // §7.9.6 Table 36: "each key_i shall be a string". An entry whose key is
    // some other object (here a number) is skipped without desynchronising
    // the key/value pairing of the entries around it.
    #[test]
    fn wrong_type_key_entry_is_skipped() {
        let bytes = build_pdf(&alloc::vec![(
            3_u32,
            b"<< /Names [(a) 1 42 2 (c) 3] >>".to_vec()
        )]);
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = tree_from(&pdf, 3);
        assert_eq!(iter_keys(&tree), alloc::vec![b"a".to_vec(), b"c".to_vec()]);
        assert!(tree.get(b"a").is_some());
        assert!(tree.get(b"c").is_some());
    }

    // §7.9.6: a root with an (empty) /Names is a valid degenerate tree, as
    // is a root with an empty /Kids. Both enumerate nothing.
    #[test]
    fn empty_tree_yields_nothing() {
        for body in [b"<< /Names [] >>".to_vec(), b"<< /Kids [] >>".to_vec()] {
            let pdf = Pdf::new(build_pdf(&alloc::vec![(3_u32, body)])).expect("pdf loads");
            let tree = tree_from(&pdf, 3);
            assert_eq!(tree.iter().count(), 0);
            assert!(tree.get(b"x").is_none());
        }
    }

    // §7.9.6: "The value associated with a given key can ... be found by
    // walking the tree in order." A genuine ≥2-level tree (root ->
    // intermediate -> leaf) must enumerate in ascending key order, and each
    // key must map to its value.
    #[test]
    fn multi_level_tree_iterates_in_key_order() {
        let bytes = build_pdf(&alloc::vec![
            (3_u32, b"<< /Kids [4 0 R 5 0 R] >>".to_vec()),
            (
                4_u32,
                b"<< /Limits [(alpha)(bravo)] /Kids [6 0 R 7 0 R] >>".to_vec()
            ),
            (
                5_u32,
                b"<< /Limits [(charlie)(delta)] /Kids [8 0 R] >>".to_vec()
            ),
            (
                6_u32,
                b"<< /Limits [(alpha)(alpha)] /Names [(alpha) 1] >>".to_vec()
            ),
            (
                7_u32,
                b"<< /Limits [(bravo)(bravo)] /Names [(bravo) 2] >>".to_vec()
            ),
            (
                8_u32,
                b"<< /Limits [(charlie)(delta)] /Names [(charlie) 3 (delta) 4] >>".to_vec(),
            ),
        ]);
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = tree_from(&pdf, 3);

        let mapped: Vec<(Vec<u8>, Option<i32>)> =
            tree.iter().map(|(k, v)| (k, v.into_i32())).collect();
        assert_eq!(
            mapped,
            alloc::vec![
                (b"alpha".to_vec(), Some(1)),
                (b"bravo".to_vec(), Some(2)),
                (b"charlie".to_vec(), Some(3)),
                (b"delta".to_vec(), Some(4)),
            ]
        );

        assert_eq!(tree.get(b"charlie").and_then(Object::into_i32), Some(3));
        // A key in the gap between the two subtrees, and one past the end.
        assert!(tree.get(b"bravocado").is_none());
        assert!(tree.get(b"echo").is_none());
    }

    // N7: a /Limits with fewer than two strings is malformed. `get` must not
    // prune a subtree that `iter` would still yield — the two must agree on
    // membership. §7.9.6 requires /Limits to be exactly two strings.
    #[test]
    fn get_agrees_with_iter_including_malformed_limits() {
        let bytes = build_pdf(&alloc::vec![
            (3_u32, b"<< /Kids [4 0 R 5 0 R] >>".to_vec()),
            (4_u32, b"<< /Limits [(a)(a)] /Names [(a) 1] >>".to_vec()),
            // Malformed /Limits (one string) on a leaf that does hold keys.
            (5_u32, b"<< /Limits [(m)] /Names [(m) 2 (n) 3] >>".to_vec()),
        ]);
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = tree_from(&pdf, 3);

        let keys = iter_keys(&tree);
        assert_eq!(
            keys,
            alloc::vec![b"a".to_vec(), b"m".to_vec(), b"n".to_vec()]
        );
        // Every key iter yields is retrievable via get (the malformed-Limits
        // subtree is not pruned away), and a non-member is not.
        for k in &keys {
            assert!(tree.get(k).is_some(), "get disagreed with iter for {k:?}");
        }
        assert!(tree.get(b"zzz").is_none());
    }

    // §7.9.6, Table 36 (p. 135): `/Limits` is "not permitted in root nodes".
    // A (spec-violating) root that nonetheless carries a `/Limits` excluding
    // its own keys must not be pruned by `get`: `iter` never reads `/Limits`,
    // so pruning would make `get` miss keys `iter` yields. The root `/Limits`
    // is ignored and the two agree.
    #[test]
    fn root_limits_is_ignored_not_pruned() {
        // A leaf-only root that holds its keys directly, plus a bogus
        // /Limits [(m)(n)] whose range excludes both of them.
        let leaf_root = build_pdf(&alloc::vec![(
            3_u32,
            b"<< /Limits [(m)(n)] /Names [(a) 1 (b) 2] >>".to_vec()
        )]);
        // A /Kids root, likewise carrying a /Limits that excludes its
        // descendants' keys — the prune must not fire at the root either.
        let kids_root = build_pdf(&alloc::vec![
            (3_u32, b"<< /Limits [(m)(n)] /Kids [4 0 R] >>".to_vec()),
            (
                4_u32,
                b"<< /Limits [(a)(b)] /Names [(a) 1 (b) 2] >>".to_vec()
            ),
        ]);
        for bytes in [leaf_root, kids_root] {
            let pdf = Pdf::new(bytes).expect("pdf loads");
            let tree = tree_from(&pdf, 3);
            assert_eq!(iter_keys(&tree), alloc::vec![b"a".to_vec(), b"b".to_vec()]);
            // get agrees with iter despite the root's excluding /Limits.
            assert!(tree.get(b"a").is_some());
            assert!(tree.get(b"b").is_some());
            assert!(tree.get(b"zzz").is_none());
        }
    }

    // The get/iter membership agreement holds only for a spec-conformant tree
    // (§7.9.6, p. 135: sorted leaves, and `/Limits` that honestly bound their
    // subtree). This pins the two documented ways a *malformed* tree makes
    // `get` miss a key `iter` still yields, so a change that narrows or widens
    // that gap must revisit the `get` doc.
    #[test]
    fn get_and_iter_diverge_on_malformed_trees() {
        // (1) Unsorted leaf: `get` stops scanning once a key passes the target
        // (the early break §7.9.6 licenses because leaves are sorted), so a key
        // placed after a larger one is unreachable — yet `iter` drains the
        // whole leaf and still yields it.
        let unsorted = build_pdf(&alloc::vec![(
            3_u32,
            b"<< /Names [(z) 1 (a) 2] >>".to_vec()
        )]);
        let pdf = Pdf::new(unsorted).expect("pdf loads");
        let tree = tree_from(&pdf, 3);
        assert_eq!(iter_keys(&tree), alloc::vec![b"z".to_vec(), b"a".to_vec()]);
        assert!(tree.get(b"z").is_some());
        // Divergence: iter yields "a" but get's scan broke at "z" > "a".
        assert!(tree.get(b"a").is_none());

        // (2) A well-formed but lying `/Limits [(a)(a)]` over a non-root leaf
        // that actually holds (z): `get` prunes the subtree, `iter` (which
        // never reads `/Limits`) still yields (z).
        let lying = build_pdf(&alloc::vec![
            (3_u32, b"<< /Kids [4 0 R] >>".to_vec()),
            (4_u32, b"<< /Limits [(a)(a)] /Names [(z) 1] >>".to_vec()),
        ]);
        let pdf = Pdf::new(lying).expect("pdf loads");
        let tree = tree_from(&pdf, 3);
        assert_eq!(iter_keys(&tree), alloc::vec![b"z".to_vec()]);
        // Divergence: iter yields "z" but get pruned the lying-/Limits subtree.
        assert!(tree.get(b"z").is_none());
    }

    // §7.9.6: the root may carry /Names directly (a single-node tree) or
    // carry /Kids pointing at leaves. Both shapes encode the same map.
    #[test]
    fn leaf_only_root_and_kids_root_yield_same_pairs() {
        let leaf_only = build_pdf(&alloc::vec![(
            3_u32,
            b"<< /Names [(x) 1 (y) 2] >>".to_vec()
        )]);
        let with_kids = build_pdf(&alloc::vec![
            (3_u32, b"<< /Kids [4 0 R] >>".to_vec()),
            (
                4_u32,
                b"<< /Limits [(x)(y)] /Names [(x) 1 (y) 2] >>".to_vec()
            ),
        ]);
        for bytes in [leaf_only, with_kids] {
            let pdf = Pdf::new(bytes).expect("pdf loads");
            let tree = tree_from(&pdf, 3);
            assert_eq!(iter_keys(&tree), alloc::vec![b"x".to_vec(), b"y".to_vec()]);
            assert!(tree.get(b"x").is_some());
            assert!(tree.get(b"y").is_some());
            assert!(tree.get(b"z").is_none());
        }
    }

    // §7.9.6 (Table 36; p. 135): "Any encoding of the keys may be used as
    // long as it is self-consistent; keys shall be compared for equality on
    // a simple byte-by-byte basis." The comparison is on the DECODED key
    // value, not the source encoding. The hex-string key `<616263>` has
    // source bytes "616263" but decodes to "abc", so lookup and iteration
    // must key on "abc". A regression comparing `String::source()` in place
    // of `as_bytes()` (the decoded value) would invert both assertions.
    #[test]
    fn keys_compared_by_decoded_value_not_source() {
        let bytes = build_pdf(&alloc::vec![(
            3_u32,
            b"<< /Names [<616263> 42] >>".to_vec()
        )]);
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = tree_from(&pdf, 3);

        // iter yields the decoded key, not the raw source bytes.
        assert_eq!(iter_keys(&tree), alloc::vec![b"abc".to_vec()]);
        // get resolves on the decoded value ...
        assert_eq!(tree.get(b"abc").and_then(Object::into_i32), Some(42));
        // ... and misses on the source encoding.
        assert!(tree.get(b"616263").is_none());
    }

    // §7.9.6 (p. 135): a node's `/Limits` are the (lexically) least and
    // greatest keys, so — like the keys — they compare by decoded value. A
    // leaf reached through a hex-encoded `/Limits` must still be found by a
    // decoded-value lookup; comparing `source()` would prune "abc" away,
    // since its source "616263" sorts before it. This pins the `/Limits`
    // prune (not just the leaf scan) to decoded-byte comparison.
    #[test]
    fn limits_prune_compares_decoded_values() {
        let bytes = build_pdf(&alloc::vec![
            (3_u32, b"<< /Kids [4 0 R] >>".to_vec()),
            (
                4_u32,
                b"<< /Limits [<616263> <616263>] /Names [<616263> 42] >>".to_vec(),
            ),
        ]);
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = tree_from(&pdf, 3);

        assert_eq!(iter_keys(&tree), alloc::vec![b"abc".to_vec()]);
        assert_eq!(tree.get(b"abc").and_then(Object::into_i32), Some(42));
        assert!(tree.get(b"616263").is_none());
    }

    // §7.9.6 (pp. 134–137) places no bound on a name tree's node count, so
    // MAX_NODE_VISITS is a processing cap on the total nodes a lookup enters.
    // The `/Kids` descent in `find_in_node` must honour it, exactly as
    // `NameTreeIter` does: once the budget is spent the scan must stop
    // resolving siblings. Before the fix the loop kept calling `next_child`
    // after the budget hit zero, parsing every remaining child dict and
    // inserting its id into the `visited` set — O(fan-out) work with the cap
    // already exhausted. With a small injected cap over a wide flat `/Kids`
    // fan-out, the number of nodes resolved (the `visited`-set size, exposed
    // by `get_bounded`) must stay within the cap, not grow to `1 + fan-out`.
    #[test]
    fn get_kids_descent_honours_visit_budget() {
        // Root whose /Kids references FANOUT distinct valid leaf dicts, each
        // holding one key that is NOT the target and a /Limits that would not
        // prune the (absent) target either.
        const FANOUT: u32 = 50;
        let mut objects: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut root: Vec<u8> = b"<< /Kids [".to_vec();
        for i in 0..FANOUT {
            let obj = 4 + i;
            root.extend_from_slice(format!("{obj} 0 R ").as_bytes());
            objects.push((
                obj,
                format!("<< /Limits [(k{i:03})(k{i:03})] /Names [(k{i:03}) {i}] >>").into_bytes(),
            ));
        }
        root.extend_from_slice(b"] >>");
        objects.push((3, root));

        let pdf = Pdf::new(build_pdf(&objects)).expect("pdf loads");
        let tree = tree_from(&pdf, 3);

        // A cap far below FANOUT: looking up an absent key ("absent" sorts
        // before every "kNNN") must stop once the budget is spent — at most
        // CAP distinct nodes resolved (root + up to CAP-1 children), never the
        // pre-fix `1 + FANOUT`.
        const CAP: usize = 5;
        let (found, resolved) = tree.get_bounded(b"absent", CAP);
        assert!(found.is_none());
        assert!(
            resolved <= CAP,
            "budget gate leaked: resolved {resolved} nodes with cap {CAP} over a \
             fan-out of {FANOUT} (pre-fix would resolve {})",
            1 + FANOUT
        );

        // The gate changes work, not results: the default-budget lookup still
        // finds a key that lies within the fan-out, hit or miss.
        assert_eq!(tree.get(b"k000").and_then(Object::into_i32), Some(0));
        assert_eq!(tree.get(b"k049").and_then(Object::into_i32), Some(49));
        assert!(tree.get(b"k050").is_none());
    }

    // A shared indirect `/Kids` array re-scanned once per sibling node is a
    // super-linear token scan — O(nodes × array length) — that the node-visit
    // budget did NOT bound: the budget capped nodes ENTERED and `visited`
    // deduped nodes DESCENDED, but nothing capped `/Kids` entries EXAMINED, so
    // a single `next_child` call scanned a whole array of already-visited refs
    // for free. `next_child` now spends one budget unit per `/Kids` entry it
    // inspects (including refs skipped as already-visited), so the whole
    // traversal examines at most `MAX_NODE_VISITS` entries and cannot be made
    // to re-scan the shared array once per node. §7.9.6 (pp. 134–137) places no
    // bound on node count, so this is the processing limit MAX_NODE_VISITS
    // documents, not a spec limit.
    #[test]
    fn get_kids_token_scan_honours_visit_budget() {
        // Root fans out to K sibling intermediate nodes; every sibling points
        // its `/Kids` at ONE shared indirect array A of M copies of a single
        // leaf L. The first sibling enters L (one distinct node); every later
        // sibling re-scans all M of A's now-already-visited refs. The refs
        // resolve to just two distinct nodes (a sibling and L), so the node-
        // entry cap alone never trips — only budgeting the token scan does.
        const K: u32 = 200;
        const M: usize = 2000;
        let a_obj = 4 + K; // the shared /Kids array
        let l_obj = 5 + K; // the single shared leaf
        let mut objects: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut root: Vec<u8> = b"<< /Kids [".to_vec();
        for i in 0..K {
            let sib = 4 + i;
            root.extend_from_slice(format!("{sib} 0 R ").as_bytes());
            // /Kids is an indirect reference to the one shared array A.
            objects.push((sib, format!("<< /Kids {a_obj} 0 R >>").into_bytes()));
        }
        root.extend_from_slice(b"] >>");
        objects.push((3, root));
        let mut a: Vec<u8> = b"[".to_vec();
        for _ in 0..M {
            a.extend_from_slice(format!("{l_obj} 0 R ").as_bytes());
        }
        a.extend_from_slice(b"]");
        objects.push((a_obj, a));
        objects.push((l_obj, b"<< /Names [(zzz) 1] >>".to_vec()));

        let pdf = Pdf::new(build_pdf(&objects)).expect("pdf loads");
        let tree = tree_from(&pdf, 3);

        // A cap ABOVE K (so a token-blind walk would enter every one of the K
        // siblings — ~K+2 nodes) but BELOW M (so once the budget also pays per
        // entry examined, the first sibling's shared array exhausts it before a
        // second sibling is reached). Looking up an absent key ("absent" sorts
        // before "zzz", so L never matches) must therefore resolve only a
        // handful of nodes, independent of K and M — not ~K.
        const CAP: usize = 1000;
        let (found, resolved) = tree.get_bounded(b"absent", CAP);
        assert!(found.is_none());
        assert!(
            resolved <= 8,
            "token scan leaked: resolved {resolved} nodes over {K} siblings \
             sharing an array of {M} refs (a token-blind walk resolves ~{})",
            K + 2
        );

        // The budget bounds work, not correctness: under the default budget the
        // one real key is still found, an absent key still missed, and the
        // shared leaf still yielded exactly once.
        assert!(tree.get(b"zzz").is_some());
        assert!(tree.get(b"absent").is_none());
        assert_eq!(iter_keys(&tree), alloc::vec![b"zzz".to_vec()]);
    }

    // The `/Names` twin of `get_kids_token_scan_honours_visit_budget`. A shared
    // indirect `/Names` array re-scanned once per leaf is a super-linear entry
    // scan — O(leaves × array length) — that the node-visit budget did NOT
    // bound before this fix: the budget capped nodes ENTERED and `visited`
    // deduped node DICTS descended, but nothing capped `/Names` entries
    // EXAMINED, so a single leaf drained a whole shared array for free.
    // `LeafCursor::next_pair` now spends one budget unit per `/Names` entry it
    // inspects, so the whole traversal examines at most `MAX_NODE_VISITS`
    // entries and cannot be made to re-scan the shared array once per leaf.
    // §7.9.6 (pp. 134–137) places no bound on node/entry count, so this is the
    // processing limit MAX_NODE_VISITS documents, not a spec limit.
    #[test]
    fn get_names_scan_honours_visit_budget() {
        // Root fans out to K sibling LEAF nodes; every sibling points its
        // `/Names` at ONE shared indirect array A of M key/value pairs and
        // carries no pruning `/Limits`. The looked-up key sorts AFTER every key
        // in A, so a budget-blind `get` descends every sibling and re-scans all
        // M entries of A each time — K × M work. The leaf dicts are distinct
        // (K node entries), but the array they share is a single object the
        // visited set never dedupes, so only budgeting the entry scan bounds it.
        const K: u32 = 200;
        const M: usize = 2000;
        let a_obj = 4 + K; // the shared /Names array
        let mut objects: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut root: Vec<u8> = b"<< /Kids [".to_vec();
        for i in 0..K {
            let sib = 4 + i;
            root.extend_from_slice(format!("{sib} 0 R ").as_bytes());
            // /Names is an indirect reference to the one shared array A; no
            // /Limits, so the sibling is never pruned.
            objects.push((sib, format!("<< /Names {a_obj} 0 R >>").into_bytes()));
        }
        root.extend_from_slice(b"] >>");
        objects.push((3, root));
        let mut a: Vec<u8> = b"[".to_vec();
        for i in 0..M {
            // Keys "k0000".."k1999" all sort before the "zzzz" lookup, so no
            // leaf ever breaks early — each is scanned to the end.
            a.extend_from_slice(format!("(k{i:04}) 1 ").as_bytes());
        }
        a.extend_from_slice(b"]");
        objects.push((a_obj, a));

        let pdf = Pdf::new(build_pdf(&objects)).expect("pdf loads");
        let tree = tree_from(&pdf, 3);

        // A cap ABOVE K (so a budget-blind walk would enter every one of the K
        // siblings — ~K+1 nodes) but BELOW M (so once the budget also pays per
        // entry examined, the first sibling's shared array exhausts it before a
        // second sibling is reached). Looking up a key that sorts after every
        // array key ("zzzz") must therefore resolve only a handful of nodes,
        // independent of K and M — not ~K.
        const CAP: usize = 1000;
        let (found, resolved) = tree.get_bounded(b"zzzz", CAP);
        assert!(found.is_none());
        assert!(
            resolved <= 8,
            "names scan leaked: resolved {resolved} nodes over {K} leaves \
             sharing an array of {M} pairs (a budget-blind walk resolves ~{})",
            K + 1
        );

        // `iter` is the identical twin: it re-drains the shared `/Names` array
        // once per leaf, so its output and work grow as K × M (here 400_000)
        // without the fix. The same per-entry budget caps it — `iter` yields no
        // more than MAX_NODE_VISITS pairs before the walk unwinds.
        assert!(
            tree.iter().count() <= MAX_NODE_VISITS,
            "iter re-scanned the shared /Names array beyond the work cap"
        );

        // The budget bounds work, not correctness: under the default budget a
        // key present in the shared array is still found, and one sorting before
        // every array key is still absent.
        assert_eq!(tree.get(b"k0000").and_then(Object::into_i32), Some(1));
        assert!(tree.get(b"aaaa").is_none());
    }

    // The `visited` set dedupes leaf DICT ids, not the `/Names` ARRAY object a
    // leaf points at, so two distinct leaves sharing one indirect `/Names`
    // array each drain it — a malformed tree (§7.9.6: the nodes' keys "shall
    // not overlap") whose bounded behaviour we pin. `iter` re-drains the shared
    // array per leaf, yielding [a b a b]; the per-entry budget (see
    // `get_names_scan_honours_visit_budget`), not array dedup, is what keeps
    // that finite. Guards against a regression that either dedupes the array
    // (collapsing to [a b]) or drops the budget (re-opening the K × M scan).
    #[test]
    fn iter_redrains_shared_names_array_per_leaf() {
        let bytes = build_pdf(&alloc::vec![
            (3_u32, b"<< /Kids [4 0 R 5 0 R] >>".to_vec()),
            // Two distinct leaf dicts, both pointing /Names at the one array A.
            (4_u32, b"<< /Names 6 0 R >>".to_vec()),
            (5_u32, b"<< /Names 6 0 R >>".to_vec()),
            (6_u32, b"[(a) 1 (b) 2]".to_vec()),
        ]);
        let pdf = Pdf::new(bytes).expect("pdf loads");
        let tree = tree_from(&pdf, 3);
        assert_eq!(
            iter_keys(&tree),
            alloc::vec![b"a".to_vec(), b"b".to_vec(), b"a".to_vec(), b"b".to_vec()]
        );
        // get finds each key in the first leaf that covers it.
        assert_eq!(tree.get(b"a").and_then(Object::into_i32), Some(1));
        assert_eq!(tree.get(b"b").and_then(Object::into_i32), Some(2));
    }
}
