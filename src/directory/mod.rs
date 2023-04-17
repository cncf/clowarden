use crate::{
    github::DynGH,
    services::{BaseRefConfigStatus, Change, ChangeDetails, ChangesSummary, DynChange},
};
use anyhow::{format_err, Context, Result};
use config::Config;
use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    fmt::Write,
    sync::Arc,
};

mod legacy;

lazy_static! {
    static ref GITHUB_URL: Regex =
        Regex::new("^https://github.com/(?P<handle>[^/]+)/?$").expect("expr in GITHUB_URL to be valid");
}

/// Type alias to represent a team name.
pub(crate) type TeamName = String;

/// Type alias to represent a username.
pub(crate) type UserName = String;

/// Type alias to represent a user full name.
pub(crate) type UserFullName = String;

/// Directory configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Directory {
    pub teams: Vec<Team>,
    pub users: Vec<User>,
}

impl Directory {
    /// Create a new directory instance from the configuration reference
    /// provided (or from the base reference when none is provided).
    pub(crate) async fn new_from_config(cfg: Arc<Config>, gh: DynGH, ref_: Option<&str>) -> Result<Self> {
        if let Ok(true) = cfg.get_bool("config.legacy.enabled") {
            let legacy_cfg =
                legacy::Cfg::get(cfg, gh, ref_).await.context("invalid directory configuration")?;
            return Ok(Self::from(legacy_cfg));
        }
        Err(format_err!(
            "only configuration in legacy format supported at the moment"
        ))
    }

    /// Returns the changes detected between this directory instance and the
    /// new one provided.
    pub(crate) fn diff(&self, new: &Directory) -> Vec<DirectoryChange> {
        let mut changes = vec![];

        // Teams
        let teams_old: HashMap<&TeamName, &Team> = self.teams.iter().map(|t| (&t.name, t)).collect();
        let teams_new: HashMap<&TeamName, &Team> = new.teams.iter().map(|t| (&t.name, t)).collect();

        // Teams added/removed
        let teams_names_old: HashSet<&TeamName> = teams_old.keys().copied().collect();
        let teams_names_new: HashSet<&TeamName> = teams_new.keys().copied().collect();
        for team_name in teams_names_new.difference(&teams_names_old) {
            changes.push(DirectoryChange::TeamAdded(teams_new[*team_name].clone()));
        }
        for team_name in teams_names_old.difference(&teams_names_new) {
            changes.push(DirectoryChange::TeamRemoved(team_name.to_string()));
        }

        // Teams maintainers and members added/removed
        for team_name in teams_new.keys() {
            if !teams_names_old.contains(team_name) {
                // New team, no need to track additional changes on it
                continue;
            }

            // Maintainers
            let maintainers_old: HashSet<&UserName> = teams_old[team_name].maintainers.iter().collect();
            let maintainers_new: HashSet<&UserName> = teams_new[team_name].maintainers.iter().collect();
            for user_name in maintainers_new.difference(&maintainers_old) {
                changes.push(DirectoryChange::TeamMaintainerAdded(
                    team_name.to_string(),
                    user_name.to_string(),
                ))
            }
            for user_name in maintainers_old.difference(&maintainers_new) {
                changes.push(DirectoryChange::TeamMaintainerRemoved(
                    team_name.to_string(),
                    user_name.to_string(),
                ))
            }

            // Members
            let members_old: HashSet<&UserName> = teams_old[team_name].members.iter().collect();
            let members_new: HashSet<&UserName> = teams_new[team_name].members.iter().collect();
            for user_name in members_new.difference(&members_old) {
                changes.push(DirectoryChange::TeamMemberAdded(
                    team_name.to_string(),
                    user_name.to_string(),
                ))
            }
            for user_name in members_old.difference(&members_new) {
                changes.push(DirectoryChange::TeamMemberRemoved(
                    team_name.to_string(),
                    user_name.to_string(),
                ))
            }
        }

        // Users
        let users_old: HashMap<&UserFullName, &User> = self.users.iter().map(|u| (&u.full_name, u)).collect();
        let users_new: HashMap<&UserFullName, &User> = new.users.iter().map(|u| (&u.full_name, u)).collect();

        // Users added/removed
        let users_fullnames_old: HashSet<&UserFullName> = users_old.keys().copied().collect();
        let users_fullnames_new: HashSet<&UserFullName> = users_new.keys().copied().collect();
        let mut users_added: Vec<&UserFullName> = vec![];
        for full_name in users_fullnames_new.difference(&users_fullnames_old) {
            changes.push(DirectoryChange::UserAdded(full_name.to_string()));
            users_added.push(full_name);
        }
        for full_name in users_fullnames_old.difference(&users_fullnames_new) {
            changes.push(DirectoryChange::UserRemoved(full_name.to_string()));
        }

        // Users updated
        for (full_name, user_new) in &users_new {
            if users_added.contains(full_name) {
                // When a user is added the change includes the full user, so
                // we don't want to track additional changes for it
                continue;
            }

            let user_old = &users_old[full_name];
            if user_new != user_old {
                changes.push(DirectoryChange::UserUpdated(full_name.to_string()));
            }
        }

        changes
    }

    /// Return a summary of the changes detected in the directory from the base
    /// to the head reference.
    pub(crate) async fn get_changes_summary(
        cfg: Arc<Config>,
        gh: DynGH,
        head_ref: &str,
    ) -> Result<ChangesSummary> {
        let directory_head = Directory::new_from_config(cfg.clone(), gh.clone(), Some(head_ref)).await?;
        let (changes, base_ref_config_status) = match Directory::new_from_config(cfg, gh, None).await {
            Ok(directory_base) => {
                let changes = directory_base
                    .diff(&directory_head)
                    .into_iter()
                    .map(|change| Box::new(change) as DynChange)
                    .collect();
                (changes, BaseRefConfigStatus::Valid)
            }
            Err(_) => (vec![], BaseRefConfigStatus::Invalid),
        };

        Ok(ChangesSummary {
            changes,
            base_ref_config_status,
        })
    }

    /// Get user identified by the user name provided.
    pub(crate) fn _get_user(&self, user_name: &str) -> Option<&User> {
        self.users.iter().find(|u| {
            if let Some(entry_user_name) = &u.user_name {
                return entry_user_name == user_name;
            }
            false
        })
    }
}

impl From<legacy::Cfg> for Directory {
    /// Create a new directory instance from the legacy configuration.
    fn from(cfg: legacy::Cfg) -> Self {
        // Teams
        let teams = cfg
            .sheriff
            .teams
            .into_iter()
            .map(|t| Team {
                name: t.name,
                maintainers: t.maintainers,
                members: t.members,
                ..Default::default()
            })
            .collect();

        // Users
        let users = if let Some(cncf) = cfg.cncf {
            cncf.people
                .into_iter()
                .map(|u| {
                    let user_name = u
                        .github
                        .as_ref()
                        .and_then(|github_url| GITHUB_URL.captures(github_url))
                        .map(|captures| captures["handle"].to_string());
                    let image_url = match u.image {
                        Some(v) if v.starts_with("https://") => Some(v),
                        Some(v) => Some(format!("https://github.com/cncf/people/raw/main/images/{v}",)),
                        None => None,
                    };
                    User {
                        full_name: u.name,
                        user_name,
                        email: u.email,
                        image_url,
                        languages: u.languages,
                        bio: u.bio,
                        website: u.website,
                        company: u.company,
                        pronouns: u.pronouns,
                        location: u.location,
                        slack_id: u.slack_id,
                        linkedin_url: u.linkedin,
                        twitter_url: u.twitter,
                        github_url: u.github,
                        wechat_url: u.wechat,
                        youtube_url: u.youtube,
                        ..Default::default()
                    }
                })
                .collect()
        } else {
            vec![]
        };

        Directory { teams, users }
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
    pub user_name: Option<UserName>,
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
    pub languages: Option<Vec<String>>,
    pub annotations: HashMap<String, String>,
}

/// Represents a change in the directory.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum DirectoryChange {
    TeamAdded(Team),
    TeamRemoved(TeamName),
    TeamMaintainerAdded(TeamName, UserName),
    TeamMaintainerRemoved(TeamName, UserName),
    TeamMemberAdded(TeamName, UserName),
    TeamMemberRemoved(TeamName, UserName),
    UserAdded(UserFullName),
    UserRemoved(UserFullName),
    UserUpdated(UserFullName),
}

impl Change for DirectoryChange {
    /// [Change::details]
    fn details(&self) -> ChangeDetails {
        match self {
            DirectoryChange::TeamAdded(team) => ChangeDetails {
                kind: "team-added".to_string(),
                extra: json!({ "team": team }),
            },
            DirectoryChange::TeamRemoved(team_name) => ChangeDetails {
                kind: "team-removed".to_string(),
                extra: json!({ "team_name": team_name }),
            },
            DirectoryChange::TeamMaintainerAdded(team_name, user_name) => ChangeDetails {
                kind: "team-maintainer-added".to_string(),
                extra: json!({ "team_name": team_name, "user_name": user_name }),
            },
            DirectoryChange::TeamMaintainerRemoved(team_name, user_name) => ChangeDetails {
                kind: "team-maintainer-removed".to_string(),
                extra: json!({ "team_name": team_name, "user_name": user_name }),
            },
            DirectoryChange::TeamMemberAdded(team_name, user_name) => ChangeDetails {
                kind: "team-member-added".to_string(),
                extra: json!({ "team_name": team_name, "user_name": user_name }),
            },
            DirectoryChange::TeamMemberRemoved(team_name, user_name) => ChangeDetails {
                kind: "team-member-removed".to_string(),
                extra: json!({ "team_name": team_name, "user_name": user_name }),
            },
            DirectoryChange::UserAdded(full_name) => ChangeDetails {
                kind: "user-added".to_string(),
                extra: json!({ "full_name": full_name }),
            },
            DirectoryChange::UserRemoved(full_name) => ChangeDetails {
                kind: "user-removed".to_string(),
                extra: json!({ "full_name": full_name }),
            },
            DirectoryChange::UserUpdated(full_name) => ChangeDetails {
                kind: "user-updated".to_string(),
                extra: json!({ "full_name": full_name }),
            },
        }
    }

    /// [Change::template_format]
    fn template_format(&self) -> Result<String> {
        let mut s = String::new();

        match self {
            DirectoryChange::TeamAdded(team) => {
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
            DirectoryChange::TeamRemoved(team_name) => {
                write!(s, "- team **{team_name}** has been *removed*")?;
            }
            DirectoryChange::TeamMaintainerAdded(team_name, user_name) => {
                write!(s, "- **{user_name}** is now a maintainer of team **{team_name}**",)?;
            }
            DirectoryChange::TeamMaintainerRemoved(team_name, user_name) => {
                write!(
                    s,
                    "- **{user_name}** is no longer a maintainer of team **{team_name}**",
                )?;
            }
            DirectoryChange::TeamMemberAdded(team_name, user_name) => {
                write!(s, "- **{user_name}** is now a member of team **{team_name}**")?;
            }
            DirectoryChange::TeamMemberRemoved(team_name, user_name) => {
                write!(
                    s,
                    "- **{user_name}** is no longer a member of team **{team_name}**",
                )?;
            }
            DirectoryChange::UserAdded(full_name) => {
                write!(s, "- user **{full_name}** has been *added*")?;
            }
            DirectoryChange::UserRemoved(full_name) => {
                write!(s, "- user **{full_name}** has been *removed*")?;
            }
            DirectoryChange::UserUpdated(full_name) => {
                write!(s, "- user **{full_name}** details have been *updated*")?;
            }
        }

        Ok(s)
    }
}
