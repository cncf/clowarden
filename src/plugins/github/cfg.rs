use super::legacy;
use crate::{
    directory::{TeamName, UserName},
    github::DynGH,
};
use anyhow::{format_err, Context, Result};
use config::Config;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

/// Type alias to represent a repository name.
pub(crate) type RepositoryName = String;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Cfg {
    pub repositories: Vec<Repository>,
}

impl Cfg {
    /// Get plugin configuration.
    pub(crate) async fn get(cfg: Arc<Config>, gh: DynGH, config_ref: Option<&str>) -> Result<Cfg> {
        if let Ok(true) = cfg.get_bool("config.legacy.enabled") {
            let sheriff_cfg = legacy::sheriff::Cfg::get(cfg, gh, config_ref)
                .await
                .context("invalid github plugin configuration")?;
            let plugin_cfg = Cfg::from(sheriff_cfg);
            return Ok(plugin_cfg);
        }
        Err(format_err!(
            "only configuration in legacy format supported at the moment"
        ))
    }

    /// Returns the changes detected on the new configuration provided.
    pub(crate) fn changes(&self, new: &Cfg) -> Vec<Change> {
        let mut changes = vec![];

        // Repositories
        let repos_old: HashMap<&RepositoryName, &Repository> =
            self.repositories.iter().map(|r| (&r.name, r)).collect();
        let repos_new: HashMap<&RepositoryName, &Repository> =
            new.repositories.iter().map(|r| (&r.name, r)).collect();

        // Helper closures to get the team's/collaborator's role
        let team_role = |collection: &HashMap<&RepositoryName, &Repository>,
                         repo_name: &RepositoryName,
                         team_name: &TeamName| {
            collection[repo_name]
                .teams
                .as_ref()
                .unwrap()
                .get(&team_name.to_string())
                .map(|r| r.to_owned())
                .unwrap_or_default()
        };
        let user_role = |collection: &HashMap<&RepositoryName, &Repository>,
                         repo_name: &RepositoryName,
                         user_name: &UserName| {
            collection[repo_name]
                .external_collaborators
                .as_ref()
                .unwrap()
                .get(&user_name.to_string())
                .map(|r| r.to_owned())
                .unwrap_or_default()
        };

        // Repositories added/removed
        let repos_names_old: HashSet<&RepositoryName> = repos_old.keys().copied().collect();
        let repos_names_new: HashSet<&RepositoryName> = repos_new.keys().copied().collect();
        let mut repos_added: Vec<&TeamName> = vec![];
        for repo_name in repos_names_new.difference(&repos_names_old) {
            changes.push(Change::RepositoryAdded(repos_new[*repo_name].clone()));
            repos_added.push(repo_name);
        }
        for repo_name in repos_names_old.difference(&repos_names_new) {
            changes.push(Change::RepositoryRemoved(repo_name.to_string()));
        }

        // Repositories teams and external collaborators added/removed
        for repo_name in repos_new.keys() {
            if repos_added.contains(repo_name) {
                // When a repo is added the change includes the full repo, so
                // we don't want to track additional changes for it
                continue;
            }

            // Teams
            let mut teams_old = HashSet::new();
            if let Some(teams) = &repos_old[repo_name].teams {
                teams_old = teams.iter().map(|(name, _)| name).collect();
            }
            let mut teams_new = HashSet::new();
            if let Some(teams) = &repos_new[repo_name].teams {
                teams_new = teams.iter().map(|(name, _)| name).collect();
            }
            for team_name in teams_new.difference(&teams_old) {
                changes.push(Change::RepositoryTeamAdded(
                    repo_name.to_string(),
                    team_name.to_string(),
                    team_role(&repos_new, repo_name, team_name),
                ))
            }
            for team_name in teams_old.difference(&teams_new) {
                changes.push(Change::RepositoryTeamRemoved(
                    repo_name.to_string(),
                    team_name.to_string(),
                ))
            }
            for team_name in &teams_new {
                let role_new = team_role(&repos_new, repo_name, team_name);
                let role_old = team_role(&repos_new, repo_name, team_name);
                if role_new != role_old {
                    changes.push(Change::RepositoryTeamRoleUpdated(
                        repo_name.to_string(),
                        team_name.to_string(),
                        role_new,
                    ))
                }
            }

            // External collaborators
            let mut collaborators_old = HashSet::new();
            if let Some(collaborators) = &repos_old[repo_name].external_collaborators {
                collaborators_old = collaborators.iter().map(|(name, _)| name).collect();
            }
            let mut collaborators_new = HashSet::new();
            if let Some(collaborators) = &repos_new[repo_name].external_collaborators {
                collaborators_new = collaborators.iter().map(|(name, _)| name).collect();
            }
            for user_name in collaborators_new.difference(&collaborators_old) {
                changes.push(Change::RepositoryCollaboratorAdded(
                    repo_name.to_string(),
                    user_name.to_string(),
                    user_role(&repos_new, repo_name, user_name),
                ))
            }
            for user_name in collaborators_old.difference(&collaborators_new) {
                changes.push(Change::RepositoryCollaboratorRemoved(
                    repo_name.to_string(),
                    user_name.to_string(),
                ))
            }
            for user_name in &collaborators_new {
                let role_new = user_role(&repos_new, repo_name, user_name);
                let role_old = user_role(&repos_new, repo_name, user_name);
                if role_new != role_old {
                    changes.push(Change::RepositoryCollaboratorRoleUpdated(
                        repo_name.to_string(),
                        user_name.to_string(),
                        role_new,
                    ))
                }
            }

            // Visibility
            let visibility_new = &repos_new[repo_name].visibility;
            let visibility_old = &repos_old[repo_name].visibility;
            if visibility_new != visibility_old {
                let visibility_new = visibility_new.clone().unwrap_or_default();
                changes.push(Change::RepositoryVisibilityUpdated(
                    repo_name.to_string(),
                    visibility_new,
                ))
            }
        }

        changes
    }
}

impl From<legacy::sheriff::Cfg> for Cfg {
    fn from(sheriff_cfg: legacy::sheriff::Cfg) -> Self {
        Self {
            repositories: sheriff_cfg.repositories,
        }
    }
}

/// Repository configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Repository {
    pub name: String,
    pub external_collaborators: Option<HashMap<UserName, Role>>,
    pub teams: Option<HashMap<TeamName, Role>>,
    pub visibility: Option<Visibility>,
}

/// Role a user or team may have assigned.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    Admin,
    Maintain,
    #[default]
    Read,
    Triage,
    Write,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Role::Admin => write!(f, "admin"),
            Role::Maintain => write!(f, "maintain"),
            Role::Read => write!(f, "read"),
            Role::Triage => write!(f, "triage"),
            Role::Write => write!(f, "write"),
        }
    }
}

/// Repository's visibility.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Visibility {
    #[default]
    Private,
    Public,
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Visibility::Private => write!(f, "private"),
            Visibility::Public => write!(f, "public"),
        }
    }
}

/// Represents a configuration change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
pub(crate) enum Change {
    RepositoryAdded(Repository),
    RepositoryRemoved(RepositoryName),
    RepositoryTeamAdded(RepositoryName, TeamName, Role),
    RepositoryTeamRemoved(RepositoryName, TeamName),
    RepositoryTeamRoleUpdated(RepositoryName, TeamName, Role),
    RepositoryCollaboratorAdded(RepositoryName, UserName, Role),
    RepositoryCollaboratorRemoved(RepositoryName, UserName),
    RepositoryCollaboratorRoleUpdated(RepositoryName, UserName, Role),
    RepositoryVisibilityUpdated(RepositoryName, Visibility),
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::RepositoryAdded(repo) => {
                write!(
                    f,
                    "- repository **{}** has been *added* (visibility: **{}**)",
                    repo.name,
                    repo.visibility.clone().unwrap_or_default()
                )?;
                if let Some(teams) = &repo.teams {
                    if !teams.is_empty() {
                        write!(f, "\n\t- Teams")?;
                        for (team_name, role) in teams.iter() {
                            write!(f, "\n\t\t- **{team_name}**: *{role}*")?;
                        }
                    }
                }
                if let Some(collaborators) = &repo.external_collaborators {
                    if !collaborators.is_empty() {
                        write!(f, "\n\t- External collaborators")?;
                        for (user_name, role) in collaborators.iter() {
                            write!(f, "\n\t\t- **{user_name}**: *{role}*")?;
                        }
                    }
                }
            }
            Change::RepositoryRemoved(repo_name) => {
                write!(f, "- repository **{}** has been *removed*", repo_name)?;
            }
            Change::RepositoryTeamAdded(repo_name, team_name, role) => {
                write!(
                    f,
                    "- team **{}** has been *added* to repository **{}** (role: **{}**)",
                    team_name, repo_name, role
                )?;
            }
            Change::RepositoryTeamRemoved(repo_name, team_name) => {
                write!(
                    f,
                    "- team **{}** has been *removed* from repository **{}**",
                    team_name, repo_name
                )?;
            }
            Change::RepositoryTeamRoleUpdated(repo_name, team_name, role) => {
                write!(
                    f,
                    "- team **{}** role in repository **{}** has been *updated* to **{}**",
                    team_name, repo_name, role
                )?;
            }
            Change::RepositoryCollaboratorAdded(repo_name, user_name, role) => {
                write!(
                    f,
                    "- user **{}** is now an external collaborator (role: **{}**) of repository **{}**",
                    user_name, role, repo_name
                )?;
            }
            Change::RepositoryCollaboratorRemoved(repo_name, user_name) => {
                write!(
                    f,
                    "- user **{}** is no longer an external collaborator of repository **{}**",
                    user_name, repo_name
                )?;
            }
            Change::RepositoryCollaboratorRoleUpdated(repo_name, user_name, role) => {
                write!(
                    f,
                    "- user **{}** role in repository **{}** has been updated to **{}**",
                    user_name, repo_name, role
                )?;
            }
            Change::RepositoryVisibilityUpdated(repo_name, visibility) => {
                write!(
                    f,
                    "- repository **{}** visibility has been updated to **{}**",
                    repo_name, visibility
                )?;
            }
        }
        Ok(())
    }
}
