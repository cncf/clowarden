//! Aggregates multiple errors into a single error value.
//!
//! ```rust
//! use anyhow::{anyhow, Result};
//! use crate::error::MultiError;
//!
//! let mut agg = MultiError::new(Some("loading config".into()));
//! agg.push(anyhow!("file missing"));
//! agg.push(anyhow!("invalid syntax"));
//!
//! if !agg.is_empty() {
//!     eprintln!("{agg}");
//! }
//! ```

use std::{
    fmt::{self, Display, Formatter, Write},
    iter::{FromIterator, IntoIterator},
};

use anyhow::{Error, Result};

/// A container that *collects* several independent errors and exposes them as one.
///
/// It is especially handy in “best-effort” loops where you want to continue
/// processing even when individual items fail.
///
/// The optional `context` string is printed **once** at the top of the
/// `Display` output.
#[derive(Debug, Default)]
pub struct MultiError {
    context: Option<String>,
    errors:  Vec<Error>,
}

impl MultiError {
    /// Creates an empty `MultiError` with optional context.
    pub fn new<C: Into<Option<String>>>(context: C) -> Self {
        Self {
            context: context.into(),
            errors:  Vec::new(),
        }
    }

    /// Returns `true` when **no** inner errors are stored.
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// Immutable view of all inner errors.
    pub fn errors(&self) -> &[Error] {
        &self.errors
    }

    /// Adds an error (or anything convertible into `anyhow::Error`).
    pub fn push<E>(&mut self, err: E)
    where
        E: Into<Error>,
    {
        self.errors.push(err.into());
    }

    /// Consumes `self`, yielding the underlying `Vec<Error>`.
    pub fn into_inner(self) -> Vec<Error> {
        self.errors
    }
}

/* -------------------------------- Impl glue ------------------------------- */

impl From<Error> for MultiError {
    fn from(err: Error) -> Self {
        Self {
            context: None,
            errors: vec![err],
        }
    }
}

impl Extend<Error> for MultiError {
    fn extend<I: IntoIterator<Item = Error>>(&mut self, iter: I) {
        self.errors.extend(iter);
    }
}

impl FromIterator<Error> for MultiError {
    fn from_iter<I: IntoIterator<Item = Error>>(iter: I) -> Self {
        Self {
            context: None,
            errors:  iter.into_iter().collect(),
        }
    }
}

impl Display for MultiError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(ctx) = &self.context {
            writeln!(f, "{ctx}:")?;
        }
        for (idx, err) in self.errors.iter().enumerate() {
            writeln!(f, "  {:>2}. {err:#}", idx + 1)?;
        }
        Ok(())
    }
}

impl std::error::Error for MultiError {}

/* ------------------------- pretty-format helper --------------------------- */

/// Human-readable, indented dump of *any* `anyhow::Error`,
/// unfolding nested `MultiError`s and cause-chains.
///
/// ```rust
/// # use anyhow::anyhow;
/// # use crate::error::{MultiError, pretty_format};
/// let mut m = MultiError::new(None);
/// m.push(anyhow!("root cause"));
/// println!("{}", pretty_format(&anyhow!(m))?);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn pretty_format(err: &Error) -> Result<String> {
    fn fmt_inner(e: &Error, depth: usize, out: &mut String) -> Result<()> {
        let indent = "  ".repeat(depth);
        if let Some(me) = e.downcast_ref::<MultiError>() {
            if let Some(ctx) = &me.context {
                writeln!(out, "{indent}{ctx}")?;
            }
            for sub in me.errors() {
                fmt_inner(sub, depth + 1, out)?;
            }
        } else {
            writeln!(out, "{indent}{e}")?;
            for cause in e.chain().skip(1) {
                writeln!(out, "{}  ↳ {}", indent, cause)?;
            }
        }
        Ok(())
    }

    let mut out = String::new();
    fmt_inner(err, 0, &mut out)?;
    Ok(out)
}
