use crate::object::{ObjectIdentifier, ObjectLike};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};

/// Maximum indirect-object reference nesting depth before parsing
/// aborts. Mirrors the xref-chain ceiling (`MAX_XREF_CHAIN_DEPTH = 256`
/// in `xref.rs`): a deep but acyclic chain of indirect references
/// (`1 0 R` -> `2 0 R` -> ...) would otherwise recurse to a native
/// stack overflow, which the parent-chain cycle check does not bound.
/// V1-FUZZ-001.
const MAX_INDIRECT_OBJECT_DEPTH: usize = 256;

#[derive(Debug, Clone)]
pub(crate) struct IndirectObject<T> {
    id: ObjectIdentifier,
    inner: T,
}

impl<T> IndirectObject<T> {
    pub(crate) fn get(self) -> T {
        self.inner
    }

    pub(crate) fn id(&self) -> &ObjectIdentifier {
        &self.id
    }
}

impl<'a, T> Readable<'a> for IndirectObject<T>
where
    T: ObjectLike<'a>,
{
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let mut ctx = ctx.clone();
        let id = r.read_without_context::<ObjectIdentifier>()?;

        if ctx.parent_chain_contains(&id) {
            warn!("cycle detected in indirect object: {id:?}");

            return None;
        }

        if ctx.parent_chain_len() >= MAX_INDIRECT_OBJECT_DEPTH {
            warn!("indirect object nesting exceeds maximum depth of {MAX_INDIRECT_OBJECT_DEPTH}");

            return None;
        }

        ctx.set_obj_number(id);
        ctx.parent_chain_push(id);
        r.skip_white_spaces_and_comments();
        let inner = r.read_with_context::<T>(&ctx)?;
        r.skip_white_spaces_and_comments();
        // We are lenient and don't require it.
        r.forward_tag(b"endobj");

        Some(Self { id, inner })
    }
}

impl<T> Skippable for IndirectObject<T>
where
    T: Skippable,
{
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        r.skip::<ObjectIdentifier>(false)?;
        r.skip_white_spaces_and_comments();
        r.skip::<T>(false)?;
        r.skip_white_spaces_and_comments();
        // We are lenient and don't require it.
        r.forward_tag(b"endobj");

        Some(())
    }
}
