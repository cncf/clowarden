use super::{legacy, service::DynSvc};
use crate::{
    directory::{Directory, DirectoryChange, Team, TeamName, UserName},
    github::DynGH,
    multierror::MultiError,
    services::{Change, ChangeDetails},
};
use anyhow::{format_err, Context, Result};
use config::Config;
use futures::stream::{self, StreamExt};
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
        if let Ok(true) = cfg.get_bool("server.config.legacy.enabled") {
            let org_admins: Vec<UserName> =
                svc.list_org_admins().await?.into_iter().map(|a| a.login).collect();

            let mut directory = Directory::new_from_config(cfg.clone(), gh.clone(), ref_).await?;

            // Team's members that are org admins are considered maintainers by
            // GitHub, so we do the same with the members defined in the config
            for team in directory.teams.iter_mut() {
                for (i, user_name) in team.members.clone().iter().enumerate() {
                    if org_admins.contains(user_name) {
                        team.members.remove(i);
                        team.maintainers.push(user_name.to_owned());
                    }
                }
            }

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
            state.validate(svc).await?;

            return Ok(state);
        }
        Err(format_err!(
            "only configuration in legacy format supported at the moment"
        ))
    }

    /// Create a new State instance from the service's actual state.
    pub(crate) async fn new_from_service(svc: DynSvc) -> Result<State> {
        let mut state = State::default();

        // Teams
        for team in stream::iter(svc.list_teams().await?)
            .map(|team| async {
                // Get maintainers and members (including pending invitations)
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

                // Setup team from info collected
                Ok(Team {
                    name: team.slug,
                    display_name: Some(team.name),
                    maintainers,
                    members,
                    ..Default::default()
                })
            })
            .buffer_unordered(8)
            .collect::<Vec<Result<Team>>>()
            .await
        {
            match team {
                Ok(team) => state.directory.teams.push(team),
                Err(err) => return Err(err.context("error getting team info")),
            }
        }

        // Repositories
        let org_admins: Vec<UserName> = svc.list_org_admins().await?.into_iter().map(|a| a.login).collect();
        for repo in stream::iter(svc.list_repositories().await?)
            .map(|repo| async {
                // Get collaborators (including pending invitations and excluding org admins)
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

                // Get teams
                let teams: HashMap<TeamName, Role> = svc
                    .list_repository_teams(&repo.name)
                    .await?
                    .into_iter()
                    .map(|t| (t.name, t.permissions.into()))
                    .collect();
                let teams = if teams.is_empty() { None } else { Some(teams) };

                // Setup repository from info collected
                Ok(Repository {
                    name: repo.name,
                    collaborators,
                    teams,
                    visibility: Some(repo.visibility.into()),
                })
            })
            .buffer_unordered(8)
            .collect::<Vec<Result<Repository>>>()
            .await
        {
            match repo {
                Ok(repo) => state.repositories.push(repo),
                Err(err) => return Err(err.context("error getting repository info")),
            }
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
    async fn validate(&self, svc: DynSvc) -> Result<()> {
        let mut merr = MultiError::new(Some("invalid github service configuration".to_string()));

        // Helper closure to get the highest role from a team membership for a
        // given user in the repository provided
        let get_highest_team_role = |repo: &Repository, user_name: &UserName| {
            let mut highest_team_role = None;
            if let Some(teams) = &repo.teams {
                for (team_name, role) in teams {
                    if let Some(team) = self.directory.get_team(team_name) {
                        if team.maintainers.contains(user_name) || team.members.contains(user_name) {
                            if highest_team_role.is_none() {
                                highest_team_role = Some((team_name.clone(), role.clone()));
                            } else {
                                let highest_role = highest_team_role.as_ref().unwrap().1.clone();
                                if role > &highest_role {
                                    highest_team_role = Some((team_name.clone(), role.clone()));
                                }
                            }
                        }
                    }
                }
            }
            highest_team_role
        };

        // Check teams' maintainers are members of the organization
        let org_members: Vec<UserName> = svc.list_org_members().await?.into_iter().map(|m| m.login).collect();
        for team in &self.directory.teams {
            for user_name in &team.maintainers {
                if !org_members.contains(user_name) {
                    merr.push(format_err!(
                        "team[{}]: {user_name} must be an organization member to be a maintainer",
                        team.name
                    ));
                }
            }
        }

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

            // Check explicitly defined collaborators haven't been assigned a
            // role with less privileges than the ones they'd have from any of
            // the teams they are members of
            if let Some(collaborators) = &repo.collaborators {
                for (user_name, user_role) in collaborators {
                    let highest_team_role = get_highest_team_role(repo, user_name);
                    if let Some((team_name, highest_team_role)) = highest_team_role {
                        if &highest_team_role > user_role {
                            merr.push(format_err!(
                                "repo[{id}]: collaborator {user_name} already has {highest_team_role} \
                                access from team {team_name}"
                            ));
                        }
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
            for team_name in teams_old.difference(&teams_new) {
                changes.push(RepositoryChange::TeamRemoved(
                    repo_name.to_string(),
                    team_name.to_string(),
                ))
            }
            for team_name in teams_new.difference(&teams_old) {
                changes.push(RepositoryChange::TeamAdded(
                    repo_name.to_string(),
                    team_name.to_string(),
                    team_role(&repos_new, repo_name, team_name),
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
            for user_name in collaborators_old.difference(&collaborators_new) {
                changes.push(RepositoryChange::CollaboratorRemoved(
                    repo_name.to_string(),
                    user_name.to_string(),
                ))
            }
            for user_name in collaborators_new.difference(&collaborators_old) {
                changes.push(RepositoryChange::CollaboratorAdded(
                    repo_name.to_string(),
                    user_name.to_string(),
                    user_role(&repos_new, repo_name, user_name),
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
#[derive(Debug, Clone, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    #[default]
    Read,
    Triage,
    Write,
    Maintain,
    Admin,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Role::Read => write!(f, "read"),
            Role::Triage => write!(f, "triage"),
            Role::Write => write!(f, "write"),
            Role::Maintain => write!(f, "maintain"),
            Role::Admin => write!(f, "admin"),
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
            RepositoryInvitationPermissions::Read => Role::Read,
            RepositoryInvitationPermissions::Triage => Role::Triage,
            RepositoryInvitationPermissions::Write => Role::Write,
            RepositoryInvitationPermissions::Maintain => Role::Maintain,
            RepositoryInvitationPermissions::Admin => Role::Admin,
            _ => Role::default(),
        }
    }
}

impl From<&Role> for RepositoryInvitationPermissions {
    fn from(role: &Role) -> Self {
        match role {
            Role::Read => RepositoryInvitationPermissions::Read,
            Role::Triage => RepositoryInvitationPermissions::Triage,
            Role::Write => RepositoryInvitationPermissions::Write,
            Role::Maintain => RepositoryInvitationPermissions::Maintain,
            Role::Admin => RepositoryInvitationPermissions::Admin,
        }
    }
}

impl From<&Role> for TeamsAddUpdateRepoPermissionsInOrgRequestPermission {
    fn from(role: &Role) -> Self {
        match role {
            Role::Read => TeamsAddUpdateRepoPermissionsInOrgRequestPermission::Pull,
            Role::Triage => TeamsAddUpdateRepoPermissionsInOrgRequestPermission::Triage,
            Role::Write => TeamsAddUpdateRepoPermissionsInOrgRequestPermission::Push,
            Role::Maintain => TeamsAddUpdateRepoPermissionsInOrgRequestPermission::Maintain,
            Role::Admin => TeamsAddUpdateRepoPermissionsInOrgRequestPermission::Admin,
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

impl From<String> for Visibility {
    fn from(value: String) -> Self {
        match value.as_str() {
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

    /// [Change::keywords]
    fn keywords(&self) -> Vec<&str> {
        match self {
            RepositoryChange::RepositoryAdded(repo) => {
                let mut keywords = vec!["repository", "added", &repo.name];
                if let Some(teams) = &repo.teams {
                    for team_name in teams.keys() {
                        keywords.push(team_name);
                    }
                }
                if let Some(collaborators) = &repo.collaborators {
                    for user_name in collaborators.keys() {
                        keywords.push(user_name);
                    }
                }
                keywords
            }
            RepositoryChange::TeamAdded(repo_name, team_name, _) => {
                vec!["repository", "team", "added", repo_name, team_name]
            }
            RepositoryChange::TeamRemoved(repo_name, team_name) => {
                vec!["repository", "team", "removed", repo_name, team_name]
            }
            RepositoryChange::TeamRoleUpdated(repo_name, team_name, _) => {
                vec!["repository", "team", "updated", repo_name, team_name]
            }
            RepositoryChange::CollaboratorAdded(repo_name, user_name, _) => {
                vec!["repository", "collaborator", "added", repo_name, user_name]
            }
            RepositoryChange::CollaboratorRemoved(repo_name, user_name) => {
                vec!["repository", "collaborator", "removed", repo_name, user_name]
            }
            RepositoryChange::CollaboratorRoleUpdated(repo_name, user_name, _) => {
                vec![
                    "repository",
                    "collaborator",
                    "role",
                    "updated",
                    repo_name,
                    user_name,
                ]
            }
            RepositoryChange::VisibilityUpdated(repo_name, _) => {
                vec!["repository", "visibility", "updated", repo_name]
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::User;

    #[test]
    fn diff_user_added_discarded() {
        let user1 = User {
            full_name: "user1".to_string(),
            ..Default::default()
        };
        let state1 = State::default();
        let state2 = State {
            directory: Directory {
                users: vec![user1],
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(state1.diff(&state2), Changes::default());
    }

    #[test]
    fn diff_repository_added() {
        let repo1 = Repository {
            name: "repo1".to_string(),
            ..Default::default()
        };
        let state1 = State::default();
        let state2 = State {
            repositories: vec![repo1.clone()],
            ..Default::default()
        };
        assert_eq!(
            state1.diff(&state2),
            Changes {
                repositories: vec![RepositoryChange::RepositoryAdded(repo1)],
                ..Default::default()
            }
        );
    }

    #[test]
    fn diff_repository_team_added() {
        let repo1 = Repository {
            name: "repo1".to_string(),
            ..Default::default()
        };
        let repo1_adding_team = Repository {
            teams: Some(HashMap::from([("team1".to_string(), Role::Write)])),
            ..repo1.clone()
        };
        let state1 = State {
            repositories: vec![repo1],
            ..Default::default()
        };
        let state2 = State {
            repositories: vec![repo1_adding_team],
            ..Default::default()
        };
        assert_eq!(
            state1.diff(&state2),
            Changes {
                repositories: vec![RepositoryChange::TeamAdded(
                    "repo1".to_string(),
                    "team1".to_string(),
                    Role::Write
                )],
                ..Default::default()
            }
        );
    }

    #[test]
    fn diff_repository_team_removed() {
        let repo1 = Repository {
            name: "repo1".to_string(),
            teams: Some(HashMap::from([("team1".to_string(), Role::Write)])),
            ..Default::default()
        };
        let repo1_removing_team = Repository {
            teams: None,
            ..repo1.clone()
        };
        let state1 = State {
            repositories: vec![repo1],
            ..Default::default()
        };
        let state2 = State {
            repositories: vec![repo1_removing_team],
            ..Default::default()
        };
        assert_eq!(
            state1.diff(&state2),
            Changes {
                repositories: vec![RepositoryChange::TeamRemoved(
                    "repo1".to_string(),
                    "team1".to_string(),
                )],
                ..Default::default()
            }
        );
    }

    #[test]
    fn diff_repository_team_role_updated() {
        let repo1 = Repository {
            name: "repo1".to_string(),
            teams: Some(HashMap::from([("team1".to_string(), Role::Write)])),
            ..Default::default()
        };
        let repo1_updating_team_role = Repository {
            teams: Some(HashMap::from([("team1".to_string(), Role::Read)])),
            ..repo1.clone()
        };
        let state1 = State {
            repositories: vec![repo1],
            ..Default::default()
        };
        let state2 = State {
            repositories: vec![repo1_updating_team_role],
            ..Default::default()
        };
        assert_eq!(
            state1.diff(&state2),
            Changes {
                repositories: vec![RepositoryChange::TeamRoleUpdated(
                    "repo1".to_string(),
                    "team1".to_string(),
                    Role::Read
                )],
                ..Default::default()
            }
        );
    }

    #[test]
    fn diff_repository_collaborator_added() {
        let repo1 = Repository {
            name: "repo1".to_string(),
            ..Default::default()
        };
        let repo1_adding_collaborator = Repository {
            collaborators: Some(HashMap::from([("user1".to_string(), Role::Write)])),
            ..repo1.clone()
        };
        let state1 = State {
            repositories: vec![repo1],
            ..Default::default()
        };
        let state2 = State {
            repositories: vec![repo1_adding_collaborator],
            ..Default::default()
        };
        assert_eq!(
            state1.diff(&state2),
            Changes {
                repositories: vec![RepositoryChange::CollaboratorAdded(
                    "repo1".to_string(),
                    "user1".to_string(),
                    Role::Write
                )],
                ..Default::default()
            }
        );
    }

    #[test]
    fn diff_repository_collaborator_removed() {
        let repo1 = Repository {
            name: "repo1".to_string(),
            collaborators: Some(HashMap::from([("user1".to_string(), Role::Write)])),
            ..Default::default()
        };
        let repo1_removing_collaborator = Repository {
            collaborators: None,
            ..repo1.clone()
        };
        let state1 = State {
            repositories: vec![repo1],
            ..Default::default()
        };
        let state2 = State {
            repositories: vec![repo1_removing_collaborator],
            ..Default::default()
        };
        assert_eq!(
            state1.diff(&state2),
            Changes {
                repositories: vec![RepositoryChange::CollaboratorRemoved(
                    "repo1".to_string(),
                    "user1".to_string(),
                )],
                ..Default::default()
            }
        );
    }

    #[test]
    fn diff_repository_collaborator_role_updated() {
        let repo1 = Repository {
            name: "repo1".to_string(),
            collaborators: Some(HashMap::from([("user1".to_string(), Role::Write)])),
            ..Default::default()
        };
        let repo1_updating_collaborator_role = Repository {
            collaborators: Some(HashMap::from([("user1".to_string(), Role::Read)])),
            ..repo1.clone()
        };
        let state1 = State {
            repositories: vec![repo1],
            ..Default::default()
        };
        let state2 = State {
            repositories: vec![repo1_updating_collaborator_role],
            ..Default::default()
        };
        assert_eq!(
            state1.diff(&state2),
            Changes {
                repositories: vec![RepositoryChange::CollaboratorRoleUpdated(
                    "repo1".to_string(),
                    "user1".to_string(),
                    Role::Read
                )],
                ..Default::default()
            }
        );
    }

    #[test]
    fn diff_repository_visibility_updated() {
        let repo1 = Repository {
            name: "repo1".to_string(),
            visibility: Some(Visibility::Private),
            ..Default::default()
        };
        let repo1_updating_visibility = Repository {
            visibility: Some(Visibility::Public),
            ..repo1.clone()
        };
        let state1 = State {
            repositories: vec![repo1],
            ..Default::default()
        };
        let state2 = State {
            repositories: vec![repo1_updating_visibility],
            ..Default::default()
        };
        assert_eq!(
            state1.diff(&state2),
            Changes {
                repositories: vec![RepositoryChange::VisibilityUpdated(
                    "repo1".to_string(),
                    Visibility::Public
                )],
                ..Default::default()
            }
        );
    }

    #[test]
    fn diff_multiple_changes() {
        let repo1 = Repository {
            name: "repo1".to_string(),
            teams: Some(HashMap::from([
                ("team1".to_string(), Role::Write),
                ("team2".to_string(), Role::Write),
            ])),
            visibility: Some(Visibility::Private),
            ..Default::default()
        };
        let repo1_updated = Repository {
            teams: Some(HashMap::from([
                ("team1".to_string(), Role::Write),
                ("team3".to_string(), Role::Write),
            ])),
            visibility: Some(Visibility::Public),
            ..repo1.clone()
        };
        let state1 = State {
            repositories: vec![repo1],
            ..Default::default()
        };
        let state2 = State {
            repositories: vec![repo1_updated],
            ..Default::default()
        };
        assert_eq!(
            state1.diff(&state2),
            Changes {
                repositories: vec![
                    RepositoryChange::TeamRemoved("repo1".to_string(), "team2".to_string()),
                    RepositoryChange::TeamAdded("repo1".to_string(), "team3".to_string(), Role::Write),
                    RepositoryChange::VisibilityUpdated("repo1".to_string(), Visibility::Public)
                ],
                ..Default::default()
            }
        );
    }
}
