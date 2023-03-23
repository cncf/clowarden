use anyhow::Error;
use std::fmt;

/// MultiError represents an error that aggregates a collection of errors.
#[derive(Debug, Default)]
pub(crate) struct MultiError {
    errors: Vec<Error>,
}

impl MultiError {
    /// Create a new MultiError instance.
    pub(crate) fn new() -> Self {
        Self { errors: vec![] }
    }

    /// Return all errors.
    pub(crate) fn errors(&self) -> Vec<&Error> {
        self.errors.iter().collect()
    }

    /// Check if at least one error has been added to the multierror instance.
    pub(crate) fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Join the provided multierror instance.
    pub(crate) fn join(&mut self, merr: MultiError) {
        self.errors.extend(merr.errors)
    }

    // Append error provided to the internal list of errors.
    pub(crate) fn push(&mut self, err: Error) {
        self.errors.push(err)
    }
}

impl From<anyhow::Error> for MultiError {
    fn from(err: Error) -> Self {
        Self { errors: vec![err] }
    }
}

impl fmt::Display for MultiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for err in &self.errors {
            writeln!(f, "- {err}")?;
        }
        Ok(())
    }
}

impl std::error::Error for MultiError {}
