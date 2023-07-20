#![warn(clippy::all, clippy::pedantic)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::similar_names
)]

pub mod cfg;
pub mod directory;
pub mod github;
pub mod multierror;
pub mod services;
