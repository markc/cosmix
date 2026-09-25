//! Headless text-buffer core for the Cosmix editor (ced): the shared buffer
//! model behind the `edit` Bus citizen (`cosmix-editd`) and, from E1, the ced
//! app and the `text_edit` scene widget.
//!
//! No Bus, no async, no clock (time arrives as `now_ms`), no I/O. Design and
//! frozen contracts: cmctl `_plan/2026-09-26-ced-e0-implementation.md`; each
//! contract is repeated as a doc comment on the type that owns it:
//! two-phase apply → [`text`], transaction order and CAS → [`buffer`],
//! transform/priority/inverse → [`ot`], undo preflight and retention →
//! [`history`], anchor mapping → [`anchor`], positions → [`pos`], wire →
//! [`wire`], refusal vocabulary → [`error`].
//!
//! Vendored msedit sources live in `vendor/msedit/` (MIT, see its README).

pub mod anchor;
pub mod buffer;
pub mod error;
pub mod history;
pub mod lang;
pub mod limits;
pub mod origin;
pub mod ot;
pub mod pos;
pub mod search;
pub mod text;
pub mod view;
pub mod wire;

mod vendor;

// editd moves each buffer into its own actor task: `Text` and `Buffer` must be `Send`.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<text::Text>();
    assert_send::<buffer::Buffer>();
};

#[cfg(test)]
mod tests;
