//! This module defines the templates used to render the comments that
//! CLOWarden will post to GitHub.

use std::collections::HashMap;

use anyhow::Error;
use askama::Template;

use clowarden_core::services::{ChangesApplied, ChangesSummary, ServiceName};

/// Template for the reconciliation completed comment.
#[derive(Template)]
#[template(path = "reconciliation-completed.md")]
pub(crate) struct ReconciliationCompleted<'a> {
    services: Vec<ServiceName>,
    changes_applied: &'a HashMap<ServiceName, ChangesApplied>,
    some_changes_applied: bool,
    errors: &'a HashMap<ServiceName, Error>,
    errors_found: bool,
}

impl<'a> ReconciliationCompleted<'a> {
    pub(crate) fn new(
        changes_applied: &'a HashMap<ServiceName, ChangesApplied>,
        errors: &'a HashMap<ServiceName, Error>,
    ) -> Self {
        let services = changes_applied.keys().chain(errors.keys()).copied().collect();
        let some_changes_applied = (|| {
            for service_changes_applied in changes_applied.values() {
                if !service_changes_applied.is_empty() {
                    return true;
                }
            }
            false
        })();
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
            some_changes_applied,
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
    changes_found: bool,
    invalid_base_ref_config_found: bool,
}

impl<'a> ValidationSucceeded<'a> {
    pub(crate) fn new(
        directory_changes: &'a ChangesSummary,
        services_changes: &'a HashMap<ServiceName, ChangesSummary>,
    ) -> Self {
        let changes_found = (|| {
            if !directory_changes.changes.is_empty() {
                return true;
            }
            for summary in services_changes.values() {
                if !summary.changes.is_empty() {
                    return true;
                }
            }
            false
        })();
        let invalid_base_ref_config_found = (|| {
            if directory_changes.base_ref_config_status.is_invalid() {
                return true;
            }
            for summary in services_changes.values() {
                if summary.base_ref_config_status.is_invalid() {
                    return true;
                }
            }
            false
        })();
        Self {
            directory_changes,
            services_changes,
            changes_found,
            invalid_base_ref_config_found,
        }
    }
}

mod filters {
    use anyhow::Error;
    use clowarden_core::multierror;

    /// Template filter that formats the error provided.
    pub(crate) fn format_error(err: &Error) -> askama::Result<String> {
        match multierror::format_error(err) {
            Ok(s) => Ok(s),
            Err(err) => Err(askama::Error::Custom(err.into())),
        }
    }
}
