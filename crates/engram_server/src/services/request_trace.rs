//! Static HTTP request navigation. Candidate source routes are not runtime observations.
pub mod api;
mod container_registrations;
mod database;
mod dbml;
mod inline_sql;
pub mod execution;
pub mod routing;
#[cfg(test)]
mod tests;
