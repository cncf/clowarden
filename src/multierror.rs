use anyhow::Error;
use std::fmt;

/// MultiError represents an error that aggregates a collection of errors.
#[derive(Debug, Default)]
pub(crate) struct MultiError {
    pub context: Option<String>,
    errors: Vec<Error>,
}

impl MultiError {
    /// Create a new MultiError instance.
    pub(crate) fn new(context: Option<String>) -> Self {
        Self {
            context,
            errors: vec![],
        }
    }

    /// Check if there is at least one error.
    pub(crate) fn contains_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Return all errors.
    pub(crate) fn errors(&self) -> Vec<&Error> {
        self.errors.iter().collect()
    }

    // Append error provided to the internal list of errors.
    pub(crate) fn push(&mut self, err: Error) {
        self.errors.push(err)
    }
}

impl From<Error> for MultiError {
    fn from(err: Error) -> Self {
        Self {
            context: None,
            errors: vec![err],
        }
    }
}

impl fmt::Display for MultiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for err in &self.errors {
            write!(f, "{:#} ", err)?;
        }
        Ok(())
    }
}

impl std::error::Error for MultiError {}
