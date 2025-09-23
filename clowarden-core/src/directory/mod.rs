//! This module defines the types used to represent a directory as well as some
//! functionality to create new instances or comparing them.

use std::{
    collections::{HashMap, HashSet},
    fmt::Write,
    sync::LazyLock,
};

use anyhow::{Context, Result, format_err};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    cfg::{Legacy, Organization},
    github::{DynGH, Source},
    services::{BaseRefConfigStatus, Change, ChangeDetails, ChangesSummary, DynChange},
};

pub mod legacy;

static GITHUB_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("^https://github.com/(?P<handle>[^/]+)/?$").expect("expr in GITHUB_URL to be valid")
});

/// Type alias to represent a team name.
pub type TeamName = String;

/// Type alias to represent a username.
pub type UserName = String;

/// Type alias to represent a user full name.
pub type UserFullName = String;

/// Directory configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Directory {
    pub teams: Vec<Team>,
    pub users: Vec<User>,
}

impl Directory {
    /// Create a new directory instance from the configuration source provided.
    pub async fn new_from_config(gh: DynGH, legacy: &Legacy, src: &Source) -> Result<Self> {
        if legacy.enabled {
            return Ok(Self::from(
                legacy::Cfg::get(gh, legacy, src).await.context("invalid directory configuration")?,
            ));
        }
        Err(format_err!(
            "only configuration in legacy format supported at the moment"
        ))
    }

    /// Returns the changes detected between this directory instance and the
    /// new one provided.
    #[must_use]
    pub fn diff(&self, new: &Directory) -> Vec<DirectoryChange> {
        let mut changes = vec![];

        // Teams
        let teams_old: HashMap<&TeamName, &Team> = self.teams.iter().map(|t| (&t.name, t)).collect();
        let teams_new: HashMap<&TeamName, &Team> = new.teams.iter().map(|t| (&t.name, t)).collect();

        // Teams added/removed
        let teams_names_old: HashSet<&TeamName> = teams_old.keys().copied().collect();
        let teams_names_new: HashSet<&TeamName> = teams_new.keys().copied().collect();
        for team_name in teams_names_old.difference(&teams_names_new) {
            changes.push(DirectoryChange::TeamRemoved((*team_name).to_string()));
        }
        for team_name in teams_names_new.difference(&teams_names_old) {
            changes.push(DirectoryChange::TeamAdded(teams_new[*team_name].clone()));
        }

        // Teams maintainers and members added/removed
        for team_name in teams_new.keys() {
            if !teams_names_old.contains(team_name) {
                // New team, no need to track additional changes on it
                continue;
            }

            let maintainers_old: HashSet<&UserName> = teams_old[team_name].maintainers.iter().collect();
            let maintainers_new: HashSet<&UserName> = teams_new[team_name].maintainers.iter().collect();
            let members_old: HashSet<&UserName> = teams_old[team_name].members.iter().collect();
            let members_new: HashSet<&UserName> = teams_new[team_name].members.iter().collect();
            for user_name in maintainers_old.difference(&maintainers_new) {
                changes.push(DirectoryChange::TeamMaintainerRemoved(
                    (*team_name).to_string(),
                    (*user_name).to_string(),
                ));
            }
            for user_name in members_old.difference(&members_new) {
                changes.push(DirectoryChange::TeamMemberRemoved(
                    (*team_name).to_string(),
                    (*user_name).to_string(),
                ));
            }
            for user_name in maintainers_new.difference(&maintainers_old) {
                changes.push(DirectoryChange::TeamMaintainerAdded(
                    (*team_name).to_string(),
                    (*user_name).to_string(),
                ));
            }
            for user_name in members_new.difference(&members_old) {
                changes.push(DirectoryChange::TeamMemberAdded(
                    (*team_name).to_string(),
                    (*user_name).to_string(),
                ));
            }
        }

        // Users
        let users_old: HashMap<&UserFullName, &User> = self.users.iter().map(|u| (&u.full_name, u)).collect();
        let users_new: HashMap<&UserFullName, &User> = new.users.iter().map(|u| (&u.full_name, u)).collect();

        // Users added/removed
        let users_fullnames_old: HashSet<&UserFullName> = users_old.keys().copied().collect();
        let users_fullnames_new: HashSet<&UserFullName> = users_new.keys().copied().collect();
        let mut users_added: Vec<&UserFullName> = vec![];
        for full_name in users_fullnames_old.difference(&users_fullnames_new) {
            changes.push(DirectoryChange::UserRemoved((*full_name).to_string()));
        }
        for full_name in users_fullnames_new.difference(&users_fullnames_old) {
            changes.push(DirectoryChange::UserAdded((*full_name).to_string()));
            users_added.push(full_name);
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
                changes.push(DirectoryChange::UserUpdated((*full_name).to_string()));
            }
        }

        changes
    }

    /// Return a summary of the changes detected in the directory from the base
    /// to the head reference.
    pub async fn get_changes_summary(
        gh: DynGH,
        org: &Organization,
        head_src: &Source,
    ) -> Result<ChangesSummary> {
        let base_src = Source::from(org);
        let directory_head = Directory::new_from_config(gh.clone(), &org.legacy, head_src).await?;
        let (changes, base_ref_config_status) =
            match Directory::new_from_config(gh, &org.legacy, &base_src).await {
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

    /// Get team identified by the team name provided.
    #[must_use]
    pub fn get_team(&self, team_name: &str) -> Option<&Team> {
        self.teams.iter().find(|t| t.name == team_name)
    }

    /// Get user identified by the user name provided.
    #[must_use]
    pub fn get_user(&self, user_name: &str) -> Option<&User> {
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
        let teams = cfg.sheriff.teams.into_iter().map(Into::into).collect();

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
pub struct Team {
    pub name: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub maintainers: Vec<UserName>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<UserName>,

    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub annotations: HashMap<String, String>,
}

impl From<legacy::sheriff::Team> for Team {
    fn from(team: legacy::sheriff::Team) -> Self {
        Team {
            name: team.name.clone(),
            maintainers: team.maintainers.clone().unwrap_or_default(),
            members: team.members.clone().unwrap_or_default(),
            ..Default::default()
        }
    }
}

/// User profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct User {
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
#[allow(clippy::large_enum_variant, clippy::module_name_repetitions)]
pub enum DirectoryChange {
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

    /// [Change::keywords]
    fn keywords(&self) -> Vec<&str> {
        match self {
            DirectoryChange::TeamAdded(team) => {
                let mut keywords = vec!["team", "added", &team.name];
                for maintainer in &team.maintainers {
                    keywords.push(maintainer);
                }
                for member in &team.members {
                    keywords.push(member);
                }
                keywords
            }
            DirectoryChange::TeamRemoved(team_name) => {
                vec!["team", "removed", team_name]
            }
            DirectoryChange::TeamMaintainerAdded(team_name, user_name) => {
                vec!["team", "maintainer", "added", team_name, user_name]
            }
            DirectoryChange::TeamMaintainerRemoved(team_name, user_name) => {
                vec!["team", "maintainer", "removed", team_name, user_name]
            }
            DirectoryChange::TeamMemberAdded(team_name, user_name) => {
                vec!["team", "member", "added", team_name, user_name]
            }
            DirectoryChange::TeamMemberRemoved(team_name, user_name) => {
                vec!["team", "member", "removed", team_name, user_name]
            }
            DirectoryChange::UserAdded(full_name) => {
                vec!["user", "added", full_name]
            }
            DirectoryChange::UserRemoved(full_name) => {
                vec!["user", "removed", full_name]
            }
            DirectoryChange::UserUpdated(full_name) => {
                vec!["user", "updated", full_name]
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_team_added() {
        let team1 = Team {
            name: "team1".to_string(),
            ..Default::default()
        };
        let dir1 = Directory::default();
        let dir2 = Directory {
            teams: vec![team1.clone()],
            ..Default::default()
        };
        assert_eq!(dir1.diff(&dir2), vec![DirectoryChange::TeamAdded(team1)]);
    }

    #[test]
    fn diff_team_removed() {
        let team1 = Team {
            name: "team1".to_string(),
            ..Default::default()
        };
        let dir1 = Directory {
            teams: vec![team1],
            ..Default::default()
        };
        let dir2 = Directory::default();
        assert_eq!(
            dir1.diff(&dir2),
            vec![DirectoryChange::TeamRemoved("team1".to_string())]
        );
    }

    #[test]
    fn diff_team_maintainer_added() {
        let team1 = Team {
            name: "team1".to_string(),
            ..Default::default()
        };
        let team1_adding_maintainer = Team {
            maintainers: vec!["user1".to_string()],
            ..team1.clone()
        };
        let dir1 = Directory {
            teams: vec![team1],
            ..Default::default()
        };
        let dir2 = Directory {
            teams: vec![team1_adding_maintainer],
            ..Default::default()
        };
        assert_eq!(
            dir1.diff(&dir2),
            vec![DirectoryChange::TeamMaintainerAdded(
                "team1".to_string(),
                "user1".to_string()
            )]
        );
    }

    #[test]
    fn diff_team_maintainer_removed() {
        let team1 = Team {
            name: "team1".to_string(),
            maintainers: vec!["user1".to_string()],
            ..Default::default()
        };
        let team1_removing_maintainer = Team {
            maintainers: vec![],
            ..team1.clone()
        };
        let dir1 = Directory {
            teams: vec![team1],
            ..Default::default()
        };
        let dir2 = Directory {
            teams: vec![team1_removing_maintainer],
            ..Default::default()
        };
        assert_eq!(
            dir1.diff(&dir2),
            vec![DirectoryChange::TeamMaintainerRemoved(
                "team1".to_string(),
                "user1".to_string()
            )]
        );
    }

    #[test]
    fn diff_team_member_added() {
        let team1 = Team {
            name: "team1".to_string(),
            ..Default::default()
        };
        let team1_adding_member = Team {
            members: vec!["user1".to_string()],
            ..team1.clone()
        };
        let dir1 = Directory {
            teams: vec![team1],
            ..Default::default()
        };
        let dir2 = Directory {
            teams: vec![team1_adding_member],
            ..Default::default()
        };
        assert_eq!(
            dir1.diff(&dir2),
            vec![DirectoryChange::TeamMemberAdded(
                "team1".to_string(),
                "user1".to_string()
            )]
        );
    }

    #[test]
    fn diff_team_member_removed() {
        let team1 = Team {
            name: "team1".to_string(),
            members: vec!["user1".to_string()],
            ..Default::default()
        };
        let team1_removing_member = Team {
            members: vec![],
            ..team1.clone()
        };
        let dir1 = Directory {
            teams: vec![team1],
            ..Default::default()
        };
        let dir2 = Directory {
            teams: vec![team1_removing_member],
            ..Default::default()
        };
        assert_eq!(
            dir1.diff(&dir2),
            vec![DirectoryChange::TeamMemberRemoved(
                "team1".to_string(),
                "user1".to_string()
            )]
        );
    }

    #[test]
    fn diff_user_added() {
        let user1 = User {
            full_name: "user1".to_string(),
            ..Default::default()
        };
        let dir1 = Directory::default();
        let dir2 = Directory {
            users: vec![user1],
            ..Default::default()
        };
        assert_eq!(
            dir1.diff(&dir2),
            vec![DirectoryChange::UserAdded("user1".to_string())]
        );
    }

    #[test]
    fn diff_user_removed() {
        let user1 = User {
            full_name: "user1".to_string(),
            ..Default::default()
        };
        let dir1 = Directory {
            users: vec![user1],
            ..Default::default()
        };
        let dir2 = Directory::default();
        assert_eq!(
            dir1.diff(&dir2),
            vec![DirectoryChange::UserRemoved("user1".to_string())]
        );
    }

    #[test]
    fn diff_user_updated() {
        let user1 = User {
            full_name: "user1".to_string(),
            ..Default::default()
        };
        let user1_updated = User {
            user_name: Some("user1".to_string()),
            ..user1.clone()
        };
        let dir1 = Directory {
            users: vec![user1],
            ..Default::default()
        };
        let dir2 = Directory {
            users: vec![user1_updated],
            ..Default::default()
        };
        assert_eq!(
            dir1.diff(&dir2),
            vec![DirectoryChange::UserUpdated("user1".to_string())]
        );
    }

    #[test]
    fn diff_multiple_changes() {
        let team1 = Team {
            name: "team1".to_string(),
            ..Default::default()
        };
        let team1_adding_maintainer = Team {
            maintainers: vec!["user1".to_string()],
            ..team1.clone()
        };
        let team2 = Team {
            name: "team2".to_string(),
            ..Default::default()
        };
        let user1 = User {
            full_name: "user1".to_string(),
            ..Default::default()
        };
        let dir1 = Directory {
            teams: vec![team1],
            users: vec![user1],
        };
        let dir2 = Directory {
            teams: vec![team1_adding_maintainer, team2.clone()],
            ..Default::default()
        };
        assert_eq!(
            dir1.diff(&dir2),
            vec![
                DirectoryChange::TeamAdded(team2),
                DirectoryChange::TeamMaintainerAdded("team1".to_string(), "user1".to_string()),
                DirectoryChange::UserRemoved("user1".to_string())
            ]
        );
    }
}
