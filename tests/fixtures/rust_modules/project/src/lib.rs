pub mod primary;
pub mod alternate;
pub mod decoy;
use crate::primary::save as persist;
use crate::alternate as backup;
pub fn run_primary() -> i32 { persist() }
pub fn run_alternate() -> i32 { backup::save() }
pub fn run_local() -> i32 { helper() }
fn helper() -> i32 { 3 }
