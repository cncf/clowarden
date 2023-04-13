use crate::services::{ChangesApplied, ChangesSummary, ServiceName};
use anyhow::Error;
use askama::Template;
use std::collections::HashMap;

/// Template for the reconciliation completed comment.
#[derive(Template)]
#[template(path = "reconciliation-completed.md")]
pub(crate) struct ReconciliationCompleted<'a> {
    services: Vec<ServiceName>,
    changes_applied: &'a HashMap<ServiceName, ChangesApplied>,
    errors: &'a HashMap<ServiceName, Error>,
    errors_found: bool,
}

impl<'a> ReconciliationCompleted<'a> {
    pub(crate) fn new(
        changes_applied: &'a HashMap<ServiceName, ChangesApplied>,
        errors: &'a HashMap<ServiceName, Error>,
    ) -> Self {
        let services = changes_applied.keys().chain(errors.keys()).map(|s| s.to_owned()).collect();
        let errors_found = (|| {
            if !errors.is_empty() {
                return true;
            }
            for service_changes_applied in changes_applied.values() {
                for change_applied in service_changes_applied {
                    if change_applied.error.is_some() {
                        return true;
                    }
                }
            }
            false
        })();
        Self {
            services,
            changes_applied,
            errors,
            errors_found,
        }
    }
}

/// Template for the validation failed comment.
#[derive(Template)]
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
#[derive(Template)]
#[template(path = "validation-succeeded.md")]
pub(crate) struct ValidationSucceeded<'a> {
    directory_changes: &'a ChangesSummary,
    services_changes: &'a HashMap<ServiceName, ChangesSummary>,
}

impl<'a> ValidationSucceeded<'a> {
    pub(crate) fn new(
        directory_changes: &'a ChangesSummary,
        services_changes: &'a HashMap<ServiceName, ChangesSummary>,
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
