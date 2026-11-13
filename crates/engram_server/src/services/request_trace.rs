//! Static HTTP request navigation. Candidate source routes are not runtime observations.
pub mod api;
mod database;
mod dbml;
pub mod execution;
pub mod routing;
#[cfg(test)]
mod tests;
