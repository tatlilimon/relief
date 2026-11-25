//! relief finds developer build output and caches that are no longer used and deletes them.
//!
//! Two promises shape the code:
//! - only things that can be produced again (rebuilt or re-downloaded) are ever deleted;
//! - "unused" is measured from the filesystem, never guessed from a name or a size.

pub mod caches;
pub mod clean;
pub mod config;
pub mod docker;
pub mod env;
pub mod fsmeta;
pub mod history;
pub mod picker;
pub mod proc;
pub mod projects;
pub mod remove;
pub mod render;
pub mod rules;
pub mod schedule;
pub mod style;
pub mod survey;
pub mod ui;
