use crate::{
    directory::Change,
    plugins::{PluginCfgChanges, PluginName},
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
    directory_changes: &'a Option<Vec<Change>>,
    plugins_changes: &'a HashMap<PluginName, PluginCfgChanges>,
}

impl<'a> ValidationSucceeded<'a> {
    pub(crate) fn new(
        directory_changes: &'a Option<Vec<Change>>,
        plugins_changes: &'a HashMap<PluginName, PluginCfgChanges>,
    ) -> Self {
        Self {
            directory_changes,
            plugins_changes,
        }
    }
}

mod filters {
    use crate::{
        directory::{self},
        multierror::MultiError,
    };
    use anyhow::{Error, Result};
    use std::fmt::Write;

    /// Template filter that formats the directory change provided.
    pub(crate) fn format_directory_change(change: &directory::Change) -> askama::Result<String> {
        let mut s = String::new();

        match change {
            directory::Change::TeamAdded(team) => {
                write!(s, "- team **{}** has been *added*", team.name)?;
                if !team.maintainers.is_empty() {
                    write!(s, "\n\t- Maintainers")?;
                    for user_name in &team.maintainers {
                        write!(s, "\n\t\t- **{user_name}**")?;
                    }
                }
                if !team.members.is_empty() {
                    write!(s, "\n\t- Members")?;
                    for user_name in &team.members {
                        write!(s, "\n\t\t- **{user_name}**")?;
                    }
                }
            }
            directory::Change::TeamRemoved(team_name) => {
                write!(s, "- team **{team_name}** has been *removed*")?;
            }
            directory::Change::TeamMaintainerAdded(team_name, user_name) => {
                write!(
                    s,
                    "- **{user_name}** is now a maintainer of team **{team_name}**",
                )?;
            }
            directory::Change::TeamMaintainerRemoved(team_name, user_name) => {
                write!(
                    s,
                    "- **{user_name}** is no longer a maintainer of team **{team_name}**",
                )?;
            }
            directory::Change::TeamMemberAdded(team_name, user_name) => {
                write!(
                    s,
                    "- **{user_name}** is now a member of team **{team_name}**"
                )?;
            }
            directory::Change::TeamMemberRemoved(team_name, user_name) => {
                write!(
                    s,
                    "- **{user_name}** is no longer a member of team **{team_name}**",
                )?;
            }
        }

        Ok(s)
    }

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
