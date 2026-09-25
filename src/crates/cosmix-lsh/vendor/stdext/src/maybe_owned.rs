// Vendored into cosmix-lsh from microsoft/edit@826b4c0 crates/stdext/src/maybe_owned.rs (upstream file carries no header; the crate is MIT, see LICENSE); see cosmix-lsh/vendor/README.md.
use std::borrow::Borrow;
use std::ops::Deref;

pub enum MaybeOwned<'a, B, O>
where
    B: ?Sized,
    O: Borrow<B>,
{
    Borrowed(&'a B),
    Owned(O),
}

impl<'a, B, O> Deref for MaybeOwned<'a, B, O>
where
    B: ?Sized,
    O: Borrow<B>,
{
    type Target = B;

    #[inline]
    fn deref(&self) -> &Self::Target {
        match self {
            MaybeOwned::Borrowed(b) => b,
            MaybeOwned::Owned(o) => o.borrow(),
        }
    }
}
