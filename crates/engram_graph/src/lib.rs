#![deny(clippy::print_stdout)]

pub mod algorithms;
pub mod analysis;
pub mod store;

pub use store::{Edge, EdgeKind, GraphStore, Node, ResolveResult};
