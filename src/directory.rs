use crate::{github::DynGH, legacy};
use anyhow::{format_err, Context, Result};
use config::Config;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    sync::Arc,
};

/// Type alias to represent a team name.
pub(crate) type TeamName = String;

/// Type alias to represent a username.
pub(crate) type UserName = String;

/// Directory configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Directory {
    pub teams: Vec<Team>,
    pub users: Vec<User>,
}

impl Directory {
    /// Create a new directory instance.
    pub(crate) async fn new(cfg: Arc<Config>, gh: DynGH, ref_: Option<&str>) -> Result<Self> {
        if let Ok(true) = cfg.get_bool("config.legacy.enabled") {
            let legacy_cfg = legacy::Cfg::get(cfg, gh, ref_)
                .await
                .context("invalid configuration (legacy format)")?;
            return Ok(Self::from(legacy_cfg));
        }
        Err(format_err!(
            "only configuration in legacy format supported at the moment"
        ))
    }

    /// Returns the changes detected on the directory provided.
    pub(crate) fn changes(&self, new: &Directory) -> Vec<Change> {
        let mut changes = vec![];

        // Teams
        let teams_old: HashMap<&TeamName, &Team> =
            self.teams.iter().map(|t| (&t.name, t)).collect();
        let teams_new: HashMap<&TeamName, &Team> = new.teams.iter().map(|t| (&t.name, t)).collect();

        // Teams added/removed
        let teams_names_old: HashSet<&TeamName> = teams_old.keys().copied().collect();
        let teams_names_new: HashSet<&TeamName> = teams_new.keys().copied().collect();
        let mut teams_added: Vec<&TeamName> = vec![];
        for team_name in teams_names_new.difference(&teams_names_old) {
            changes.push(Change::TeamAdded(teams_new[*team_name].clone()));
            teams_added.push(team_name);
        }
        for team_name in teams_names_old.difference(&teams_names_new) {
            changes.push(Change::TeamRemoved(team_name.to_string()));
        }

        // Teams maintainers and members added/removed
        for team_name in teams_new.keys() {
            if teams_added.contains(team_name) {
                // When a team is added the change includes the full team, so
                // we don't want another change for maintainer or member changes
                continue;
            }

            // Maintainers
            let maintainers_old: HashSet<&UserName> =
                teams_old[team_name].maintainers.iter().collect();
            let maintainers_new: HashSet<&UserName> =
                teams_new[team_name].maintainers.iter().collect();
            for user_name in maintainers_new.difference(&maintainers_old) {
                changes.push(Change::TeamMaintainerAdded(
                    team_name.to_string(),
                    user_name.to_string(),
                ))
            }
            for user_name in maintainers_old.difference(&maintainers_new) {
                changes.push(Change::TeamMaintainerRemoved(
                    team_name.to_string(),
                    user_name.to_string(),
                ))
            }

            // Members
            let members_old: HashSet<&UserName> = teams_old[team_name].members.iter().collect();
            let members_new: HashSet<&UserName> = teams_new[team_name].members.iter().collect();
            for user_name in members_new.difference(&members_old) {
                changes.push(Change::TeamMemberAdded(
                    team_name.to_string(),
                    user_name.to_string(),
                ))
            }
            for user_name in members_old.difference(&members_new) {
                changes.push(Change::TeamMemberRemoved(
                    team_name.to_string(),
                    user_name.to_string(),
                ))
            }
        }

        changes
    }
}

/// Team configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Team {
    pub name: String,
    pub display_name: Option<String>,
    pub maintainers: Vec<UserName>,
    pub members: Vec<UserName>,
    pub annotations: HashMap<String, String>,
}

/// User profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct User {
    pub full_name: String,
    pub user_name: Option<String>,
    pub email: Option<String>,
    pub image_url: Option<String>,
    pub bio: Option<String>,
    pub website: Option<String>,
    pub company: Option<String>,
    pub pronouns: Option<String>,
    pub location: Option<String>,
    pub slack_id: Option<String>,
    pub linkedin_url: Option<String>,
    pub twitter_url: Option<String>,
    pub github_url: Option<String>,
    pub wechat_url: Option<String>,
    pub youtube_url: Option<String>,
    pub languages: Vec<String>,
    pub annotations: HashMap<String, String>,
}

/// Represents a change in the directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
pub(crate) enum Change {
    TeamAdded(Team),
    TeamRemoved(TeamName),
    TeamMaintainerAdded(TeamName, UserName),
    TeamMaintainerRemoved(TeamName, UserName),
    TeamMemberAdded(TeamName, UserName),
    TeamMemberRemoved(TeamName, UserName),
}

impl Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TeamAdded(team) => {
                write!(f, "- **{}** has been *added*", team.name)?;
                if !team.maintainers.is_empty() {
                    write!(f, "\n\t- Maintainers")?;
                    for user_name in &team.maintainers {
                        write!(f, "\n\t\t- **{user_name}**")?;
                    }
                }
                if !team.members.is_empty() {
                    write!(f, "\n\t- Members")?;
                    for user_name in &team.members {
                        write!(f, "\n\t\t- **{user_name}**")?;
                    }
                }
                Ok(())
            }
            Self::TeamRemoved(team_name) => write!(f, "- **{team_name}** has been *removed*"),
            Self::TeamMaintainerAdded(team_name, user_name) => write!(
                f,
                "- **{user_name}** is now a maintainer of **{team_name}**",
            ),
            Self::TeamMaintainerRemoved(team_name, user_name) => write!(
                f,
                "- **{user_name}** is no longer a maintainer of **{team_name}**",
            ),
            Self::TeamMemberAdded(team_name, user_name) => {
                write!(f, "- **{user_name}** is now a member of **{team_name}**",)
            }
            Self::TeamMemberRemoved(team_name, user_name) => write!(
                f,
                "- **{user_name}** is no longer a member of **{team_name}**",
            ),
        }
    }
}
