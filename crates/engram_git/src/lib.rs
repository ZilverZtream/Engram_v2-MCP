#![deny(clippy::print_stdout)]

pub mod collector;
pub mod history;
pub mod temporal;

pub use collector::GitCollector;
pub use history::{AntiPatternDoc, GitUpdateResult, GitWalker};
