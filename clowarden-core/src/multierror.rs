//! This module defines an error type that can aggregate multiple errors.

use std::fmt::{self, Write};

use anyhow::{Error, Result};

/// MultiError represents an error that aggregates a collection of errors.
#[derive(Debug, Default)]
pub struct MultiError {
    pub context: Option<String>,
    errors: Vec<Error>,
}

impl MultiError {
    /// Create a new MultiError instance.
    #[must_use]
    pub fn new(context: Option<String>) -> Self {
        Self {
            context,
            errors: vec![],
        }
    }

    /// Check if there is at least one error.
    #[must_use]
    pub fn contains_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Return all errors.
    #[must_use]
    pub fn errors(&self) -> Vec<&Error> {
        self.errors.iter().collect()
    }

    // Append error provided to the internal list of errors.
    pub fn push(&mut self, err: Error) {
        self.errors.push(err);
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
            write!(f, "{err:#} ")?;
        }
        Ok(())
    }
}

impl std::error::Error for MultiError {}

/// Format the error provided recursively.
#[allow(clippy::missing_errors_doc)]
pub fn format_error(err: &Error) -> Result<String> {
    fn format_error(err: &Error, depth: usize, s: &mut String) -> Result<()> {
        if let Some(merr) = err.downcast_ref::<MultiError>() {
            let mut next_depth = depth;
            if let Some(context) = &merr.context {
                write!(s, "\n{}- {context}", "\t".repeat(depth))?;
                next_depth += 1;
            }
            for err in &merr.errors() {
                format_error(err, next_depth, s)?;
            }
        } else {
            write!(s, "\n{}- {err}", "\t".repeat(depth))?;
            if err.chain().skip(1).count() > 0 {
                let mut depth = depth;
                for cause in err.chain().skip(1) {
                    depth += 1;
                    write!(s, "\n{}- {cause}", "\t".repeat(depth))?;
                }
            }
        }
        Ok(())
    }

    let mut s = String::new();
    format_error(err, 0, &mut s)?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use anyhow::{Error, format_err};

    use super::{MultiError, format_error};

    #[test]
    fn test_format_error_flattens_context_less_multierrors() {
        // Setup the error shape returned when loading a configuration
        let mut rules = MultiError::new(None);
        rules.push(format_err!("rule violated"));
        let mut merr = MultiError::new(Some("invalid config".to_string()));
        merr.push(rules.into());
        merr.push(format_err!("boom").context("error fetching file"));
        let err = Error::from(merr).context("outer context");

        // Check children are indented once and cause chains go one level deeper
        assert_eq!(
            format_error(&err).unwrap(),
            "\n- invalid config\n\t- rule violated\n\t- error fetching file\n\t\t- boom"
        );
    }

    #[test]
    fn test_format_error_indents_context_chain_in_nested_multierror() {
        // Setup the error shape built by the validate job
        let mut svc = MultiError::new(Some("svc".to_string()));
        svc.push(format_err!("root").context("mid").context("top"));
        let mut merr = MultiError::new(None);
        merr.push(svc.into());

        // Check each context level is one level deeper than the previous one
        assert_eq!(
            format_error(&merr.into()).unwrap(),
            "\n- svc\n\t- top\n\t\t- mid\n\t\t\t- root"
        );
    }
}
