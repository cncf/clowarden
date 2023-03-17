use crate::{directory::Change, multierror::MultiError};
use anyhow::Error;
use askama::Template;

/// Template for the validation failed comment.
#[derive(Debug, Template)]
#[template(path = "validation-failed.md")]
pub(crate) struct ValidationFailed<'a> {
    errors: Vec<&'a Error>,
}

impl<'a> ValidationFailed<'a> {
    pub(crate) fn new(err: &'a Error) -> Self {
        let errors = match err.downcast_ref::<MultiError>() {
            Some(merr) => merr.errors(),
            None => vec![err],
        };
        Self { errors }
    }
}

/// Template for the validation succeeded comment.
#[derive(Debug, Template)]
#[template(path = "validation-succeeded.md")]
pub(crate) struct ValidationSucceeded<'a> {
    changes: &'a Vec<Change>,
}

impl<'a> ValidationSucceeded<'a> {
    pub(crate) fn new(changes: &'a Vec<Change>) -> Self {
        Self { changes }
    }
}
