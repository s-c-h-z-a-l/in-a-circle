//! The headless engine: paths, content classification, the catalog, the
//! planner and the executor.

pub mod catalog;
pub mod classify;
pub mod exec;
pub mod journal;
pub mod menu;
pub mod output;
pub mod sandbox;
mod sys;
pub mod trash;
pub mod xdg;
