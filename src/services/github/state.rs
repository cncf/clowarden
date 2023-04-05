use super::legacy;
use crate::{
    directory::{self, Directory, TeamName, UserName},
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

/// GitHub's service state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct State {
    pub directory: Directory,
    pub repositories: Vec<Repository>,
}

impl State {
    /// Create a new State instance from the configuration reference provided.
    pub(crate) async fn new_from_config(
        cfg: Arc<Config>,
        gh: DynGH,
        ref_: Option<&str>,
    ) -> Result<State> {
        if let Ok(true) = cfg.get_bool("config.legacy.enabled") {
            let directory = Directory::new(cfg.clone(), gh.clone(), ref_).await?;
            let repositories = legacy::sheriff::Cfg::get(cfg, gh, ref_)
                .await
                .context("invalid github service configuration")?
                .repositories;
            let state = State {
                directory,
                repositories,
            };
            return Ok(state);
        }
        Err(format_err!(
            "only configuration in legacy format supported at the moment"
        ))
    }

    /// Returns the changes detected on the new state provided.
    pub(crate) fn changes(&self, new: &State) -> Changes {
        Changes {
            directory: self.directory.changes(&new.directory),
            repositories: self.repositories_changes(&new.repositories),
        }
    }

    /// Returns the changes detected between two lists of repositories.
    fn repositories_changes(&self, new: &[Repository]) -> Vec<RepositoryChange> {
        let mut changes = vec![];

        // Repositories
        let repos_old: HashMap<&RepositoryName, &Repository> =
            self.repositories.iter().map(|r| (&r.name, r)).collect();
        let repos_new: HashMap<&RepositoryName, &Repository> =
            new.iter().map(|r| (&r.name, r)).collect();

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
            changes.push(RepositoryChange::Added(repos_new[*repo_name].clone()));
            repos_added.push(repo_name);
        }
        for repo_name in repos_names_old.difference(&repos_names_new) {
            changes.push(RepositoryChange::Removed(repo_name.to_string()));
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
                changes.push(RepositoryChange::TeamAdded(
                    repo_name.to_string(),
                    team_name.to_string(),
                    team_role(&repos_new, repo_name, team_name),
                ))
            }
            for team_name in teams_old.difference(&teams_new) {
                changes.push(RepositoryChange::TeamRemoved(
                    repo_name.to_string(),
                    team_name.to_string(),
                ))
            }
            for team_name in &teams_new {
                let role_new = team_role(&repos_new, repo_name, team_name);
                let role_old = team_role(&repos_new, repo_name, team_name);
                if role_new != role_old {
                    changes.push(RepositoryChange::TeamRoleUpdated(
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
                changes.push(RepositoryChange::CollaboratorAdded(
                    repo_name.to_string(),
                    user_name.to_string(),
                    user_role(&repos_new, repo_name, user_name),
                ))
            }
            for user_name in collaborators_old.difference(&collaborators_new) {
                changes.push(RepositoryChange::CollaboratorRemoved(
                    repo_name.to_string(),
                    user_name.to_string(),
                ))
            }
            for user_name in &collaborators_new {
                let role_new = user_role(&repos_new, repo_name, user_name);
                let role_old = user_role(&repos_new, repo_name, user_name);
                if role_new != role_old {
                    changes.push(RepositoryChange::CollaboratorRoleUpdated(
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
                changes.push(RepositoryChange::VisibilityUpdated(
                    repo_name.to_string(),
                    visibility_new,
                ))
            }
        }

        changes
    }
}

/// Repository information.
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

/// Repository visibility.
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

/// Represents the changes between two states.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Changes {
    pub directory: Vec<directory::Change>,
    pub repositories: Vec<RepositoryChange>,
}

/// Represents a repository change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepositoryChange {
    Added(Repository),
    Removed(RepositoryName),
    TeamAdded(RepositoryName, TeamName, Role),
    TeamRemoved(RepositoryName, TeamName),
    TeamRoleUpdated(RepositoryName, TeamName, Role),
    CollaboratorAdded(RepositoryName, UserName, Role),
    CollaboratorRemoved(RepositoryName, UserName),
    CollaboratorRoleUpdated(RepositoryName, UserName, Role),
    VisibilityUpdated(RepositoryName, Visibility),
}

impl fmt::Display for RepositoryChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepositoryChange::Added(repo) => {
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
            RepositoryChange::Removed(repo_name) => {
                write!(f, "- repository **{}** has been *removed*", repo_name)?;
            }
            RepositoryChange::TeamAdded(repo_name, team_name, role) => {
                write!(
                    f,
                    "- team **{}** has been *added* to repository **{}** (role: **{}**)",
                    team_name, repo_name, role
                )?;
            }
            RepositoryChange::TeamRemoved(repo_name, team_name) => {
                write!(
                    f,
                    "- team **{}** has been *removed* from repository **{}**",
                    team_name, repo_name
                )?;
            }
            RepositoryChange::TeamRoleUpdated(repo_name, team_name, role) => {
                write!(
                    f,
                    "- team **{}** role in repository **{}** has been *updated* to **{}**",
                    team_name, repo_name, role
                )?;
            }
            RepositoryChange::CollaboratorAdded(repo_name, user_name, role) => {
                write!(
                    f,
                    "- user **{}** is now an external collaborator (role: **{}**) of repository **{}**",
                    user_name, role, repo_name
                )?;
            }
            RepositoryChange::CollaboratorRemoved(repo_name, user_name) => {
                write!(
                    f,
                    "- user **{}** is no longer an external collaborator of repository **{}**",
                    user_name, repo_name
                )?;
            }
            RepositoryChange::CollaboratorRoleUpdated(repo_name, user_name, role) => {
                write!(
                    f,
                    "- user **{}** role in repository **{}** has been updated to **{}**",
                    user_name, repo_name, role
                )?;
            }
            RepositoryChange::VisibilityUpdated(repo_name, visibility) => {
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
