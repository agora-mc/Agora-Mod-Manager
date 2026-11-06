//! Who owns a named thing, and what part it plays (MASTER_SPEC §30.4, §30.11).
//!
//! The evaluator asks a [`RefResolver`](super::RefResolver) about every ref a parser emits.
//! [`Resolver`] answers from an [`InstanceView`], a snapshot of one instance at crash time, using a
//! crash pack's `ref_maps` and `roles`. Nothing here opens a file: a ref value from an untrusted
//! log is only ever a lookup key.

mod index;
mod instance;
mod resolve;
mod view;

#[cfg(test)]
mod tests;

pub use index::ArchiveIndex;
pub use instance::{from_instance, InstanceViewError};
pub use resolve::{Attribution, Resolver};
pub use view::{InstanceView, LayerOwner, LayerView, Listing};
