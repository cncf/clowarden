use anyhow::Error;
use std::fmt;

/// MultiError represents an error that aggregates a collection of errors.
#[derive(Debug, Default)]
pub(crate) struct MultiError {
    errors: Vec<Error>,
}

impl MultiError {
    /// Return all errors.
    pub(crate) fn errors(&self) -> Vec<&Error> {
        self.errors.iter().collect()
    }

    // Append error provided to the internal list of errors.
    pub(crate) fn _push(&mut self, err: Error) {
        self.errors.push(err)
    }
}

impl fmt::Display for MultiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for err in &self.errors {
            write!(f, "- {}", err)?;
        }
        Ok(())
    }
}

impl std::error::Error for MultiError {}
