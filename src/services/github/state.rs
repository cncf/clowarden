use super::{legacy, service::DynSvc};
use crate::{
    directory::{Directory, DirectoryChange, Team, TeamName, UserName},
    github::DynGH,
    multierror::MultiError,
    services::{Change, ChangeDetails},
};
use anyhow::{format_err, Context, Result};
use config::Config;
use octorust::types::{
    OrgMembershipState, RepositoryInvitationPermissions, RepositoryPermissions, TeamMembershipRole,
    TeamPermissions, TeamsAddUpdateRepoPermissionsInOrgRequestPermission,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Write},
    sync::Arc,
};

/// Type alias to represent a repository name.
pub(crate) type RepositoryName = String;

/// Type alias to represent a repository invitation_id.
pub(crate) type RepositoryInvitationId = i64;

/// GitHub's service state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct State {
    pub directory: Directory,
    pub repositories: Vec<Repository>,
}

impl State {
    /// Create a new State instance from the configuration reference provided
    /// (or from the base reference when none is provided).
    pub(crate) async fn new_from_config(
        cfg: Arc<Config>,
        gh: DynGH,
        svc: DynSvc,
        ref_: Option<&str>,
    ) -> Result<State> {
        if let Ok(true) = cfg.get_bool("config.legacy.enabled") {
            let org_admins: Vec<UserName> =
                svc.list_org_admins().await?.into_iter().map(|a| a.login).collect();

            let directory = Directory::new_from_config(cfg.clone(), gh.clone(), ref_).await?;
            let repositories = legacy::sheriff::Cfg::get(cfg, gh, ref_)
                .await
                .context("invalid github service configuration")?
                .repositories
                .into_iter()
                .map(|mut r| {
                    // Set default visibility when none is provided
                    if r.visibility.is_none() {
                        r.visibility = Some(Visibility::default());
                    }

                    // Remove organization admins from collaborators list
                    if let Some(collaborators) = r.collaborators {
                        r.collaborators = Some(
                            collaborators
                                .into_iter()
                                .filter(|(user_name, _)| !org_admins.contains(user_name))
                                .collect(),
                        );
                    }

                    r
                })
                .collect();

            let state = State {
                directory,
                repositories,
            };
            state.validate()?;

            return Ok(state);
        }
        Err(format_err!(
            "only configuration in legacy format supported at the moment"
        ))
    }

    /// Create a new State instance from the service's actual state.
    pub(crate) async fn new_from_service(svc: DynSvc) -> Result<State> {
        let mut state = State::default();

        // TODO: increase concurrency for requests done to service below

        // Teams
        for team in svc.list_teams().await? {
            // Maintainers and members (including pending invitations)
            let mut maintainers: Vec<UserName> =
                svc.list_team_maintainers(&team.slug).await?.into_iter().map(|u| u.login).collect();
            let mut members: Vec<UserName> =
                svc.list_team_members(&team.slug).await?.into_iter().map(|u| u.login).collect();
            for invitation in svc.list_team_invitations(&team.slug).await?.into_iter() {
                let membership = svc.get_team_membership(&team.slug, &invitation.login).await?;
                if membership.state == OrgMembershipState::Pending {
                    match membership.role {
                        TeamMembershipRole::Maintainer => maintainers.push(invitation.login),
                        TeamMembershipRole::Member => members.push(invitation.login),
                        _ => {}
                    }
                }
            }

            state.directory.teams.push(Team {
                name: team.slug,
                display_name: Some(team.name),
                maintainers,
                members,
                ..Default::default()
            });
        }

        // Repositories
        let org_admins: Vec<UserName> = svc.list_org_admins().await?.into_iter().map(|a| a.login).collect();
        for repo in svc.list_repositories().await? {
            // Collaborators (including pending invitations and excluding org admins)
            let mut collaborators: HashMap<UserName, Role> = svc
                .list_repository_collaborators(&repo.name)
                .await?
                .into_iter()
                .filter(|c| !org_admins.contains(&c.login))
                .map(|c| (c.login, c.permissions.into()))
                .collect();
            for invitation in svc.list_repository_invitations(&repo.name).await?.into_iter() {
                if let Some(invitee) = invitation.invitee {
                    collaborators.insert(invitee.login, invitation.permissions.into());
                }
            }
            let collaborators = if collaborators.is_empty() {
                None
            } else {
                Some(collaborators)
            };

            // Teams
            let teams: HashMap<TeamName, Role> = svc
                .list_repository_teams(&repo.name)
                .await?
                .into_iter()
                .map(|t| (t.name, t.permissions.into()))
                .collect();
            let teams = if teams.is_empty() { None } else { Some(teams) };

            state.repositories.push(Repository {
                name: repo.name,
                collaborators,
                teams,
                visibility: Some(repo.visibility.as_str().into()),
            });
        }

        Ok(state)
    }

    /// Returns the changes detected between this state instance and the new
    /// one provided.
    pub(crate) fn diff(&self, new: &State) -> Changes {
        Changes {
            directory: self
                .directory
                .diff(&new.directory)
                .into_iter()
                .filter(|change| {
                    // We are not interested in users' changes
                    !matches!(
                        change,
                        DirectoryChange::UserAdded(_)
                            | DirectoryChange::UserRemoved(_)
                            | DirectoryChange::UserUpdated(_)
                    )
                })
                .collect(),
            repositories: State::repositories_diff(&self.repositories, &new.repositories),
        }
    }

    /// Validate state.
    fn validate(&self) -> Result<()> {
        let mut merr = MultiError::new(Some("invalid github service configuration".to_string()));

        for (i, repo) in self.repositories.iter().enumerate() {
            // Define id to be used in subsequent error messages. When
            // available, it'll be the repo name. Otherwise we'll use its
            // index on the list.
            let id = if repo.name.is_empty() {
                format!("{}", i)
            } else {
                repo.name.clone()
            };

            // Check teams used in repositories exist in directory
            let teams_in_directory: Vec<&TeamName> = self.directory.teams.iter().map(|t| &t.name).collect();
            if let Some(teams) = &repo.teams {
                for team_name in teams.keys() {
                    if !teams_in_directory.contains(&team_name) {
                        merr.push(format_err!(
                            "repo[{id}]: team {team_name} does not exist in directory"
                        ));
                    }
                }
            }
        }

        if merr.contains_errors() {
            return Err(merr.into());
        }
        Ok(())
    }

    /// Returns the changes detected between two lists of repositories.
    fn repositories_diff(old: &[Repository], new: &[Repository]) -> Vec<RepositoryChange> {
        let mut changes = vec![];

        // Repositories
        let repos_old: HashMap<&RepositoryName, &Repository> = old.iter().map(|r| (&r.name, r)).collect();
        let repos_new: HashMap<&RepositoryName, &Repository> = new.iter().map(|r| (&r.name, r)).collect();

        // Helper closures to get the team's/collaborator's role
        let team_role = |collection: &HashMap<&RepositoryName, &Repository>,
                         repo_name: &RepositoryName,
                         team_name: &TeamName| {
            if let Some(teams) = collection[repo_name].teams.as_ref() {
                return teams.get(&team_name.to_string()).map(|r| r.to_owned()).unwrap_or_default();
            }
            Role::default()
        };
        let user_role = |collection: &HashMap<&RepositoryName, &Repository>,
                         repo_name: &RepositoryName,
                         user_name: &UserName| {
            if let Some(collaborators) = collection[repo_name].collaborators.as_ref() {
                return collaborators.get(&user_name.to_string()).map(|r| r.to_owned()).unwrap_or_default();
            }
            Role::default()
        };

        // Repositories added
        let repos_names_old: HashSet<&RepositoryName> = repos_old.keys().copied().collect();
        let repos_names_new: HashSet<&RepositoryName> = repos_new.keys().copied().collect();
        for repo_name in repos_names_new.difference(&repos_names_old) {
            changes.push(RepositoryChange::RepositoryAdded(repos_new[*repo_name].clone()));
        }

        // Repositories teams and collaborators added/removed
        for repo_name in repos_new.keys() {
            if !repos_names_old.contains(repo_name) {
                // New repo, no need to track additional changes on it
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
                if !teams_old.contains(team_name) {
                    // New team, no need to track additional changes on it
                    continue;
                }
                let role_new = team_role(&repos_new, repo_name, team_name);
                let role_old = team_role(&repos_old, repo_name, team_name);
                if role_new != role_old {
                    changes.push(RepositoryChange::TeamRoleUpdated(
                        repo_name.to_string(),
                        team_name.to_string(),
                        role_new,
                    ))
                }
            }

            // Collaborators
            let mut collaborators_old = HashSet::new();
            if let Some(collaborators) = &repos_old[repo_name].collaborators {
                collaborators_old = collaborators.iter().map(|(name, _)| name).collect();
            }
            let mut collaborators_new = HashSet::new();
            if let Some(collaborators) = &repos_new[repo_name].collaborators {
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
                if !collaborators_old.contains(user_name) {
                    // New collaborator, no need to track additional changes on it
                    continue;
                }
                let role_new = user_role(&repos_new, repo_name, user_name);
                let role_old = user_role(&repos_old, repo_name, user_name);
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

    #[serde(alias = "external_collaborators", skip_serializing_if = "Option::is_none")]
    pub collaborators: Option<HashMap<UserName, Role>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub teams: Option<HashMap<TeamName, Role>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
}

/// Role a user or team may have been assigned.
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

impl From<Option<RepositoryPermissions>> for Role {
    fn from(permissions: Option<RepositoryPermissions>) -> Self {
        match permissions {
            Some(p) if p.admin => Role::Admin,
            Some(p) if p.maintain => Role::Maintain,
            Some(p) if p.push => Role::Write,
            Some(p) if p.triage => Role::Triage,
            Some(p) if p.pull => Role::Read,
            Some(_) => Role::default(),
            None => Role::default(),
        }
    }
}

impl From<RepositoryInvitationPermissions> for Role {
    fn from(permissions: RepositoryInvitationPermissions) -> Self {
        match permissions {
            RepositoryInvitationPermissions::Admin => Role::Admin,
            RepositoryInvitationPermissions::Maintain => Role::Maintain,
            RepositoryInvitationPermissions::Write => Role::Write,
            RepositoryInvitationPermissions::Triage => Role::Triage,
            RepositoryInvitationPermissions::Read => Role::Read,
            _ => Role::default(),
        }
    }
}

impl From<&Role> for RepositoryInvitationPermissions {
    fn from(role: &Role) -> Self {
        match role {
            Role::Admin => RepositoryInvitationPermissions::Admin,
            Role::Maintain => RepositoryInvitationPermissions::Maintain,
            Role::Write => RepositoryInvitationPermissions::Write,
            Role::Triage => RepositoryInvitationPermissions::Triage,
            Role::Read => RepositoryInvitationPermissions::Read,
        }
    }
}

impl From<&Role> for TeamsAddUpdateRepoPermissionsInOrgRequestPermission {
    fn from(role: &Role) -> Self {
        match role {
            Role::Admin => TeamsAddUpdateRepoPermissionsInOrgRequestPermission::Admin,
            Role::Maintain => TeamsAddUpdateRepoPermissionsInOrgRequestPermission::Maintain,
            Role::Write => TeamsAddUpdateRepoPermissionsInOrgRequestPermission::Push,
            Role::Triage => TeamsAddUpdateRepoPermissionsInOrgRequestPermission::Triage,
            Role::Read => TeamsAddUpdateRepoPermissionsInOrgRequestPermission::Pull,
        }
    }
}

impl From<Option<TeamPermissions>> for Role {
    fn from(permissions: Option<TeamPermissions>) -> Self {
        match permissions {
            Some(p) if p.admin => Role::Admin,
            Some(p) if p.maintain => Role::Maintain,
            Some(p) if p.push => Role::Write,
            Some(p) if p.triage => Role::Triage,
            Some(p) if p.pull => Role::Read,
            Some(_) => Role::default(),
            None => Role::default(),
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

impl From<&str> for Visibility {
    fn from(value: &str) -> Self {
        match value {
            "private" => Visibility::Private,
            "public" => Visibility::Public,
            _ => Visibility::default(),
        }
    }
}

/// Represents the changes between two states.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Changes {
    pub directory: Vec<DirectoryChange>,
    pub repositories: Vec<RepositoryChange>,
}

/// Represents a repository change.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RepositoryChange {
    RepositoryAdded(Repository),
    TeamAdded(RepositoryName, TeamName, Role),
    TeamRemoved(RepositoryName, TeamName),
    TeamRoleUpdated(RepositoryName, TeamName, Role),
    CollaboratorAdded(RepositoryName, UserName, Role),
    CollaboratorRemoved(RepositoryName, UserName),
    CollaboratorRoleUpdated(RepositoryName, UserName, Role),
    VisibilityUpdated(RepositoryName, Visibility),
}

impl Change for RepositoryChange {
    /// [Change::details]
    fn details(&self) -> ChangeDetails {
        match self {
            RepositoryChange::RepositoryAdded(repo) => ChangeDetails {
                kind: "repository-added".to_string(),
                extra: json!({ "repo": repo }),
            },
            RepositoryChange::TeamAdded(repo_name, team_name, role) => ChangeDetails {
                kind: "repository-team-added".to_string(),
                extra: json!({ "repo_name": repo_name, "team_name": team_name, "role": role }),
            },
            RepositoryChange::TeamRemoved(repo_name, team_name) => ChangeDetails {
                kind: "repository-team-removed".to_string(),
                extra: json!({ "repo_name": repo_name, "team_name": team_name }),
            },
            RepositoryChange::TeamRoleUpdated(repo_name, team_name, role) => ChangeDetails {
                kind: "repository-team-role-updated".to_string(),
                extra: json!({ "repo_name": repo_name, "team_name": team_name, "role": role }),
            },
            RepositoryChange::CollaboratorAdded(repo_name, user_name, role) => ChangeDetails {
                kind: "repository-collaborator-added".to_string(),
                extra: json!({ "repo_name": repo_name, "user_name": user_name, "role": role }),
            },
            RepositoryChange::CollaboratorRemoved(repo_name, user_name) => ChangeDetails {
                kind: "repository-collaborator-removed".to_string(),
                extra: json!({ "repo_name": repo_name, "user_name": user_name }),
            },
            RepositoryChange::CollaboratorRoleUpdated(repo_name, user_name, role) => ChangeDetails {
                kind: "repository-collaborator-role-updated".to_string(),
                extra: json!({ "repo_name": repo_name, "user_name": user_name, "role": role }),
            },
            RepositoryChange::VisibilityUpdated(repo_name, visibility) => ChangeDetails {
                kind: "repository-visibility-updated".to_string(),
                extra: json!({ "repo_name": repo_name, "visibility": visibility }),
            },
        }
    }

    /// [Change::template_format]
    fn template_format(&self) -> Result<String> {
        let mut s = String::new();

        match self {
            RepositoryChange::RepositoryAdded(repo) => {
                write!(
                    s,
                    "- repository **{}** has been *added* (visibility: **{}**)",
                    repo.name,
                    repo.visibility.clone().unwrap_or_default()
                )?;
                if let Some(teams) = &repo.teams {
                    if !teams.is_empty() {
                        write!(s, "\n\t- Teams")?;
                        for (team_name, role) in teams.iter() {
                            write!(s, "\n\t\t- **{team_name}**: *{role}*")?;
                        }
                    }
                }
                if let Some(collaborators) = &repo.collaborators {
                    if !collaborators.is_empty() {
                        write!(s, "\n\t- Collaborators")?;
                        for (user_name, role) in collaborators.iter() {
                            write!(s, "\n\t\t- **{user_name}**: *{role}*")?;
                        }
                    }
                }
            }
            RepositoryChange::TeamAdded(repo_name, team_name, role) => {
                write!(
                    s,
                    "- team **{}** has been *added* to repository **{}** (role: **{}**)",
                    team_name, repo_name, role
                )?;
            }
            RepositoryChange::TeamRemoved(repo_name, team_name) => {
                write!(
                    s,
                    "- team **{}** has been *removed* from repository **{}**",
                    team_name, repo_name
                )?;
            }
            RepositoryChange::TeamRoleUpdated(repo_name, team_name, role) => {
                write!(
                    s,
                    "- team **{}** role in repository **{}** has been *updated* to **{}**",
                    team_name, repo_name, role
                )?;
            }
            RepositoryChange::CollaboratorAdded(repo_name, user_name, role) => {
                write!(
                    s,
                    "- user **{}** is now a collaborator (role: **{}**) of repository **{}**",
                    user_name, role, repo_name
                )?;
            }
            RepositoryChange::CollaboratorRemoved(repo_name, user_name) => {
                write!(
                    s,
                    "- user **{}** is no longer a collaborator of repository **{}**",
                    user_name, repo_name
                )?;
            }
            RepositoryChange::CollaboratorRoleUpdated(repo_name, user_name, role) => {
                write!(
                    s,
                    "- user **{}** role in repository **{}** has been updated to **{}**",
                    user_name, repo_name, role
                )?;
            }
            RepositoryChange::VisibilityUpdated(repo_name, visibility) => {
                write!(
                    s,
                    "- repository **{}** visibility has been updated to **{}**",
                    repo_name, visibility
                )?;
            }
        }

        Ok(s)
    }
}
