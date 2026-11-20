//! Static HTTP request navigation. Candidate source routes are not runtime observations.
pub mod api;
mod api_attributes;
mod container_registrations;
pub(crate) mod bulk_copy;
pub(crate) mod client_callers;
mod database;
mod dbml;
mod inline_sql;
pub(crate) mod identity_insert;
pub(crate) mod sql_commands;
mod web_methods;
mod http_handlers;
pub mod execution;
pub mod control_events;
mod control_calls;
pub mod control_routes;
pub mod master_pages;
pub mod base_pages;
pub mod routing;
pub(crate) mod triggers;
pub(crate) mod value_lineage;
#[cfg(test)]
mod tests;
