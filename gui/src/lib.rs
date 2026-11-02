//! hopper's desktop viewer on skelter: a berth dock of source, stack, locals, timeline, calls and
//! a query pane, talking only to an `Arc<dyn Debugger>` through background jobs.

pub mod app;
pub mod calls;
pub mod document;
pub mod gutter;
pub mod jobs;
pub mod nav;
pub mod rows;
pub mod run;
pub mod source;
pub mod style;
pub mod timeline;
pub mod timeline_model;
pub mod tree;
pub mod ui;
