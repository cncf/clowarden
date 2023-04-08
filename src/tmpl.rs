use crate::{
    directory,
    services::{self, ServiceName},
};
use anyhow::Error;
use askama::Template;
use std::collections::HashMap;

/// Template for the validation failed comment.
#[derive(Debug, Template)]
#[template(path = "validation-failed.md")]
pub(crate) struct ValidationFailed<'a> {
    err: &'a Error,
}

impl<'a> ValidationFailed<'a> {
    pub(crate) fn new(err: &'a Error) -> Self {
        Self { err }
    }
}

/// Template for the validation succeeded comment.
#[derive(Debug, Template)]
#[template(path = "validation-succeeded.md")]
pub(crate) struct ValidationSucceeded<'a> {
    directory_changes: &'a directory::ChangesSummary,
    services_changes: &'a HashMap<ServiceName, services::ChangesSummary>,
}

impl<'a> ValidationSucceeded<'a> {
    pub(crate) fn new(
        directory_changes: &'a directory::ChangesSummary,
        services_changes: &'a HashMap<ServiceName, services::ChangesSummary>,
    ) -> Self {
        Self {
            directory_changes,
            services_changes,
        }
    }
}

mod filters {
    use crate::multierror::MultiError;
    use anyhow::{Error, Result};
    use std::fmt::Write;

    /// Template filter that formats the error provided.
    pub(crate) fn format_error(err: &Error) -> askama::Result<String> {
        // Helper function that formats the error provided recursively.
        fn format_error(err: &Error, depth: usize, s: &mut String) -> Result<()> {
            match err.downcast_ref::<MultiError>() {
                Some(merr) => {
                    let mut next_depth = depth;
                    if let Some(context) = &merr.context {
                        write!(s, "\n{}- {context}", "\t".repeat(depth))?;
                        next_depth += 1;
                    }
                    for err in &merr.errors() {
                        format_error(err, next_depth, s)?;
                    }
                }
                None => {
                    write!(s, "\n{}- {err}", "\t".repeat(depth))?;
                    if err.chain().skip(1).count() > 0 {
                        let mut depth = depth;
                        for cause in err.chain().skip(1) {
                            depth += 1;
                            write!(s, "\n{}- {cause}", "\t".repeat(depth))?;
                        }
                    }
                }
            };
            Ok(())
        }

        let mut s = String::new();
        if let Err(err) = format_error(err, 0, &mut s) {
            return Err(askama::Error::Custom(err.into()));
        }
        Ok(s)
    }
}
