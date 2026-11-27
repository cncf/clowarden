//! This module defines the types used to represent the state of the GitHub
//! service, as well as the functionality to create new instances from the
//! configuration or the service, and validating and comparing them.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::{self, Write},
    sync::LazyLock,
};

use anyhow::{Context, Result, format_err};
use octorust::types::{
    MinimalRepository, OrgMembershipState, RepositoryInvitation, RepositoryInvitationPermissions,
    RepositoryPermissions, Team as OctorustTeam, TeamMembershipRole, TeamPermissions,
    TeamsAddUpdateRepoPermissionsInOrgRequestPermission,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::debug;

use crate::{
    cfg::Legacy,
    directory::{Directory, DirectoryChange, Team, TeamName, UserName},
    github::{DynGH, Source},
    multierror::MultiError,
    services::{Change, ChangeDetails},
};

use super::{
    legacy,
    service::{Ctx, DynSvc},
};

/// Prefix GitHub adds to the slug of enterprise teams. Organization teams
/// cannot use it, as their slugs do not contain colons.
const ENTERPRISE_TEAM_SLUG_PREFIX: &str = "ent:";

/// Regular expression to match temporary private forks created for GitHub
/// security advisories.
static GHSA_TEMP_FORK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("^(.+)-ghsa(-[23456789cfghjmpqrvwx]{4}){3}$").expect("expr in GHSA_TEMP_FORK to be valid")
});

/// Type alias to represent a repository name.
pub type RepositoryName = String;

/// Type alias to represent a repository invitation_id.
pub type RepositoryInvitationId = i64;

/// Actual service state plus repository invitations (active and expired)
/// captured during the scan so reconciliation can reuse them without
/// re-fetching.
#[derive(Debug, Clone, Default)]
pub struct ActualStateSnapshot {
    /// Repository invitations (active and expired) keyed by repository name.
    pub repository_invitations: HashMap<RepositoryName, Vec<RepositoryInvitation>>,
    /// Normalized actual GitHub service state.
    pub state: State,
}

/// Organization-level GitHub metadata shared across config loading,
/// validation, and narrowed actual-state reads within a single job.
#[derive(Debug, Clone, Default)]
pub struct ServiceMetadata {
    /// Organization administrators.
    pub org_admins: Vec<UserName>,
    /// Organization members.
    pub org_members: Vec<UserName>,
    /// Repositories currently present in the organization.
    pub repositories: Vec<MinimalRepository>,
}

impl ServiceMetadata {
    /// Load organization-level admins, members, and repositories once for a
    /// job.
    pub async fn load(svc: &DynSvc, ctx: &Ctx) -> Result<Self> {
        // Fetch org admins, members, and repositories once for reuse
        let org_admins = svc.list_org_admins(ctx).await?.into_iter().map(|a| a.login).collect();
        let repositories = svc.list_repositories(ctx).await?;
        let org_members = svc.list_org_members(ctx).await?.into_iter().map(|m| m.login).collect();

        Ok(Self {
            org_admins,
            org_members,
            repositories,
        })
    }
}

/// GitHub's service state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub directory: Directory,
    pub repositories: Vec<Repository>,
}

impl State {
    /// Returns the changes detected between this state instance and the new
    /// one provided.
    #[must_use]
    pub fn diff(&self, new: &State) -> Changes {
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

    /// Create a new State instance from the configuration reference provided.
    pub async fn new_from_config(
        gh: DynGH,
        svc: DynSvc,
        legacy: &Legacy,
        ctx: &Ctx,
        src: &Source,
    ) -> Result<State> {
        if !legacy.enabled {
            return Err(format_err!(
                "only configuration in legacy format supported at the moment"
            ));
        }

        // Load organization admins and repositories needed to normalize config
        let org_admins: Vec<UserName> =
            svc.list_org_admins(ctx).await?.into_iter().map(|a| a.login).collect();
        let repositories = svc.list_repositories(ctx).await?;

        // Build state from configuration files before fetching org members
        let state = Self::build_from_config(gh, legacy, &org_admins, &repositories, src).await?;

        // Validate state against organization membership
        let org_members: Vec<UserName> =
            svc.list_org_members(ctx).await?.into_iter().map(|m| m.login).collect();
        state.validate(&org_members)?;

        Ok(state)
    }

    /// Create a new State instance from the configuration reference using
    /// pre-fetched [`ServiceMetadata`].
    pub async fn new_from_config_with_metadata(
        gh: DynGH,
        legacy: &Legacy,
        metadata: &ServiceMetadata,
        src: &Source,
    ) -> Result<State> {
        if !legacy.enabled {
            return Err(format_err!(
                "only configuration in legacy format supported at the moment"
            ));
        }

        let state =
            Self::build_from_config(gh, legacy, &metadata.org_admins, &metadata.repositories, src).await?;
        state.validate(&metadata.org_members)?;
        Ok(state)
    }

    /// Create a new State instance from the service's full actual state.
    pub async fn new_from_service(svc: DynSvc, ctx: &Ctx) -> Result<State> {
        let snapshot = Self::load_from_service(&svc, ctx, None, None, None).await?;
        Ok(snapshot.state)
    }

    /// Create a narrowed actual-state snapshot using pre-fetched metadata and
    /// `desired` so unmanaged repositories and teams scheduled for deletion do
    /// not incur unnecessary per-resource API calls.
    pub async fn new_from_service_for_desired(
        svc: DynSvc,
        ctx: &Ctx,
        desired: &State,
        metadata: &ServiceMetadata,
    ) -> Result<ActualStateSnapshot> {
        let desired_teams: HashSet<&str> = desired.directory.teams.iter().map(|t| t.name.as_str()).collect();
        let desired_repos: HashSet<&str> = desired.repositories.iter().map(|r| r.name.as_str()).collect();

        Self::load_from_service(
            &svc,
            ctx,
            Some(metadata),
            Some(&desired_teams),
            Some(&desired_repos),
        )
        .await
    }

    /// Build a normalized [`State`] from configuration files without running
    /// membership validation yet.
    async fn build_from_config(
        gh: DynGH,
        legacy: &Legacy,
        org_admins: &[UserName],
        repositories_in_service: &[MinimalRepository],
        src: &Source,
    ) -> Result<State> {
        // Helper closure to check if a repository has been archived
        let is_repository_archived = |repo_name: &RepositoryName| {
            for repo in repositories_in_service {
                if &repo.name == repo_name {
                    return repo.archived;
                }
            }
            false
        };

        // Prepare directory
        let mut directory = Directory::new_from_config(gh.clone(), legacy, src).await?;

        // Team's members that are org admins are considered maintainers by
        // GitHub, so we do the same with the members defined in the config
        for team in &mut directory.teams {
            let mut org_admins_members = vec![];
            for user_name in &team.members.clone() {
                if org_admins.contains(user_name) {
                    org_admins_members.push(user_name.clone());
                    team.maintainers.push(user_name.clone());
                }
            }
            team.members.retain(|user_name| !org_admins_members.contains(user_name));
        }

        // Prepare repositories, excluding archived repos and GHSA temp forks
        let repositories = legacy::sheriff::Cfg::get(gh, src, &legacy.sheriff_permissions_path)
            .await
            .context("invalid github service configuration")?
            .repositories
            .into_iter()
            .filter(|r| !is_repository_archived(&r.name) && !GHSA_TEMP_FORK.is_match(&r.name))
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

        Ok(State {
            directory,
            repositories,
        })
    }

    /// Record team, collaborator, and visibility changes for an existing
    /// repository present in both states.
    fn diff_repository(old: &Repository, new: &Repository, changes: &mut Vec<RepositoryChange>) {
        // Diff repository teams
        Self::diff_role_map(
            old.teams.as_ref(),
            new.teams.as_ref(),
            |team_name| RepositoryChange::TeamRemoved(new.name.clone(), team_name),
            |team_name, role| RepositoryChange::TeamAdded(new.name.clone(), team_name, role),
            |team_name, role| RepositoryChange::TeamRoleUpdated(new.name.clone(), team_name, role),
            changes,
        );

        // Diff repository collaborators
        Self::diff_role_map(
            old.collaborators.as_ref(),
            new.collaborators.as_ref(),
            |user_name| RepositoryChange::CollaboratorRemoved(new.name.clone(), user_name),
            |user_name, role| RepositoryChange::CollaboratorAdded(new.name.clone(), user_name, role),
            |user_name, role| RepositoryChange::CollaboratorRoleUpdated(new.name.clone(), user_name, role),
            changes,
        );

        // Diff repository visibility
        if new.visibility != old.visibility {
            let visibility_new = new.visibility.clone().unwrap_or_default();
            changes.push(RepositoryChange::VisibilityUpdated(
                new.name.clone(),
                visibility_new,
            ));
        }
    }

    /// Compute removed, added, and role-updated changes between two optional
    /// role maps.
    fn diff_role_map<K, FRemoved, FAdded, FUpdated>(
        old: Option<&BTreeMap<K, Role>>,
        new: Option<&BTreeMap<K, Role>>,
        on_removed: FRemoved,
        on_added: FAdded,
        on_updated: FUpdated,
        changes: &mut Vec<RepositoryChange>,
    ) where
        K: Ord + Clone,
        FRemoved: Fn(K) -> RepositoryChange,
        FAdded: Fn(K, Role) -> RepositoryChange,
        FUpdated: Fn(K, Role) -> RepositoryChange,
    {
        let empty = BTreeMap::new();
        let old = old.unwrap_or(&empty);
        let new = new.unwrap_or(&empty);

        // Record entries removed from the new map
        for key in old.keys() {
            if !new.contains_key(key) {
                changes.push(on_removed(key.clone()));
            }
        }

        // Record entries added to the new map
        for (key, role_new) in new {
            if !old.contains_key(key) {
                changes.push(on_added(key.clone(), role_new.clone()));
            }
        }

        // Record entries whose role changed between the two maps
        for (key, role_new) in new {
            if let Some(role_old) = old.get(key)
                && role_new != role_old
            {
                changes.push(on_updated(key.clone(), role_new.clone()));
            }
        }
    }

    /// Load actual state from GitHub, optionally scoping per-team and per-repo
    /// detail calls to `desired_teams` and `desired_repos`.
    async fn load_from_service(
        svc: &DynSvc,
        ctx: &Ctx,
        metadata: Option<&ServiceMetadata>,
        desired_teams: Option<&HashSet<&str>>,
        desired_repos: Option<&HashSet<&str>>,
    ) -> Result<ActualStateSnapshot> {
        // Load organization teams fail-fast before querying repository state
        let teams = Self::load_teams_from_service(svc, ctx, desired_teams).await?;

        // Reuse pre-fetched admins/repos when available, otherwise fetch after
        // teams succeed so team read errors fail fast
        let (owned_admins, owned_repos);
        let (org_admins, repositories_in_service) = if let Some(metadata) = metadata {
            (&metadata.org_admins[..], &metadata.repositories[..])
        } else {
            owned_admins = svc.list_org_admins(ctx).await?.into_iter().map(|a| a.login).collect::<Vec<_>>();
            owned_repos = svc.list_repositories(ctx).await?;
            (&owned_admins[..], &owned_repos[..])
        };

        // Load repositories and capture their invitations for reconciliation
        let (repositories, repository_invitations) = Self::load_repositories_from_service(
            svc,
            ctx,
            org_admins,
            repositories_in_service,
            desired_repos,
        )
        .await?;

        Ok(ActualStateSnapshot {
            repository_invitations,
            state: State {
                directory: Directory {
                    teams,
                    ..Default::default()
                },
                repositories,
            },
        })
    }

    /// Load managed repositories and their invitations (active and expired)
    /// from GitHub, skipping archived repos, GHSA temp forks, and unmanaged
    /// repositories.
    async fn load_repositories_from_service(
        svc: &DynSvc,
        ctx: &Ctx,
        org_admins: &[UserName],
        repositories_in_service: &[MinimalRepository],
        desired_repos: Option<&HashSet<&str>>,
    ) -> Result<(
        Vec<Repository>,
        HashMap<RepositoryName, Vec<RepositoryInvitation>>,
    )> {
        let mut repositories = vec![];
        let mut repository_invitations = HashMap::new();

        // Inspect repositories fail-fast, skipping archived, GHSA temp forks,
        // and unmanaged repositories when scoped to desired state
        for repo in repositories_in_service {
            if repo.archived || GHSA_TEMP_FORK.is_match(&repo.name) {
                continue;
            }
            if desired_repos.is_some_and(|desired| !desired.contains(repo.name.as_str())) {
                continue;
            }

            // Load repository details and keep invitations for reconciliation
            let (repository, invitations) =
                Self::load_repository_from_service(svc, ctx, org_admins, repo).await?;
            repository_invitations.insert(repo.name.clone(), invitations);
            repositories.push(repository);
        }
        repositories.sort_by(|a, b| a.name.cmp(&b.name));

        Ok((repositories, repository_invitations))
    }

    /// Load collaborators, invitations, and team permissions for a single
    /// repository from GitHub.
    async fn load_repository_from_service(
        svc: &DynSvc,
        ctx: &Ctx,
        org_admins: &[UserName],
        repo: &MinimalRepository,
    ) -> Result<(Repository, Vec<RepositoryInvitation>)> {
        // Get collaborators (including pending invitations and excluding org admins)
        let mut collaborators: BTreeMap<UserName, Role> = svc
            .list_repository_collaborators(ctx, &repo.name)
            .await
            .with_context(|| format!("error listing repository {} collaborators", repo.name))
            .context("error getting repository info")?
            .into_iter()
            .filter(|c| !org_admins.contains(&c.login))
            .map(|c| (c.login, c.permissions.into()))
            .collect();
        // Load repository invitations, keeping all of them in the snapshot
        // while excluding expired ones from active collaborator access so
        // reconciliation can delete and recreate them
        let invitations = svc
            .list_repository_invitations(ctx, &repo.name)
            .await
            .with_context(|| format!("error listing repository {} invitations", repo.name))
            .context("error getting repository info")?;
        for invitation in &invitations {
            if !invitation.expired
                && let Some(invitee) = &invitation.invitee
            {
                collaborators.insert(invitee.login.clone(), invitation.permissions.clone().into());
            }
        }
        let collaborators = if collaborators.is_empty() {
            None
        } else {
            Some(collaborators)
        };

        // Get teams (access granted to enterprise teams is left out)
        let mut teams: BTreeMap<TeamName, Role> = BTreeMap::new();
        for team in svc
            .list_repository_teams(ctx, &repo.name)
            .await
            .with_context(|| format!("error listing repository {} teams", repo.name))
            .context("error getting repository info")?
        {
            let role: Role = team.permissions.into();
            if is_enterprise_team(&team.slug) {
                debug!(
                    repo = repo.name,
                    team = team.slug,
                    %role,
                    "ignoring enterprise team access to repository"
                );
                continue;
            }
            teams.insert(team.slug, role);
        }
        let teams = if teams.is_empty() { None } else { Some(teams) };

        let repository = Repository {
            name: repo.name.clone(),
            collaborators,
            teams,
            visibility: Some(repo.visibility.clone().into()),
        };
        Ok((repository, invitations))
    }

    /// Load maintainers, members, and pending invitations for a single team
    /// from GitHub.
    async fn load_team_from_service(svc: &DynSvc, ctx: &Ctx, team: OctorustTeam) -> Result<Team> {
        // Load confirmed maintainers and members
        let mut maintainers: Vec<UserName> = svc
            .list_team_maintainers(ctx, &team.slug)
            .await
            .with_context(|| format!("error listing team {} maintainers", team.slug))
            .context("error getting team info")?
            .into_iter()
            .map(|u| u.login)
            .collect();
        let mut members: Vec<UserName> = svc
            .list_team_members(ctx, &team.slug)
            .await
            .with_context(|| format!("error listing team {} members", team.slug))
            .context("error getting team info")?
            .into_iter()
            .map(|u| u.login)
            .collect();

        // Include pending invitations under their target role
        for invitation in svc
            .list_team_invitations(ctx, &team.slug)
            .await
            .with_context(|| format!("error listing team {} invitations", team.slug))
            .context("error getting team info")?
        {
            // Skip invitations without login (e.g. email invitations)
            if invitation.login.is_empty() {
                continue;
            }

            let membership = svc
                .get_team_membership(ctx, &team.slug, &invitation.login)
                .await
                .with_context(|| {
                    format!(
                        "error getting team {} membership of {}",
                        team.slug, invitation.login
                    )
                })
                .context("error getting team info")?;
            if membership.state == OrgMembershipState::Pending {
                match membership.role {
                    TeamMembershipRole::Maintainer => maintainers.push(invitation.login),
                    TeamMembershipRole::Member => members.push(invitation.login),
                    TeamMembershipRole::FallthroughString => {}
                }
            }
        }
        maintainers.sort();
        members.sort();

        Ok(Team {
            name: team.slug,
            display_name: Some(team.name),
            maintainers,
            members,
            ..Default::default()
        })
    }

    /// Load organization teams from GitHub, filtering out enterprise teams and
    /// skipping detail calls for teams absent from `desired_teams`.
    async fn load_teams_from_service(
        svc: &DynSvc,
        ctx: &Ctx,
        desired_teams: Option<&HashSet<&str>>,
    ) -> Result<Vec<Team>> {
        // Partition enterprise teams out before inspecting organization teams
        let (enterprise_teams, org_teams): (Vec<_>, Vec<_>) =
            svc.list_teams(ctx).await?.into_iter().partition(|team| is_enterprise_team(&team.slug));
        if !enterprise_teams.is_empty() {
            let slugs: Vec<&str> = enterprise_teams.iter().map(|team| team.slug.as_str()).collect();
            debug!(teams = ?slugs, "ignoring enterprise teams");
        }

        // Inspect organization teams fail-fast, skipping detail calls for
        // teams absent from desired state (Directory::diff only needs slug)
        let mut teams = Vec::with_capacity(org_teams.len());
        for team in org_teams {
            if desired_teams.is_some_and(|desired| !desired.contains(team.slug.as_str())) {
                teams.push(Team {
                    name: team.slug,
                    display_name: Some(team.name),
                    ..Default::default()
                });
                continue;
            }

            teams.push(Self::load_team_from_service(svc, ctx, team).await?);
        }
        teams.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(teams)
    }

    /// Returns the changes detected between two lists of repositories.
    fn repositories_diff(old: &[Repository], new: &[Repository]) -> Vec<RepositoryChange> {
        let mut changes = vec![];
        let repos_old: BTreeMap<&RepositoryName, &Repository> = old.iter().map(|r| (&r.name, r)).collect();
        let repos_new: BTreeMap<&RepositoryName, &Repository> = new.iter().map(|r| (&r.name, r)).collect();

        // Record repositories added in the new state
        for (repo_name, repo_new) in &repos_new {
            if !repos_old.contains_key(repo_name) {
                changes.push(RepositoryChange::RepositoryAdded((*repo_new).clone()));
            }
        }

        // Diff teams, collaborators, and visibility on existing repositories
        for (repo_name, repo_new) in &repos_new {
            if let Some(repo_old) = repos_old.get(repo_name) {
                Self::diff_repository(repo_old, repo_new, &mut changes);
            }
        }

        changes
    }

    /// Validate state.
    fn validate(&self, org_members: &[UserName]) -> Result<()> {
        let mut merr = MultiError::new(Some("invalid github service configuration".to_string()));

        // Helper closure to get the highest role from a team membership for a
        // given user in the repository provided
        let get_highest_team_role = |repo: &Repository, user_name: &UserName| {
            let mut highest_team_role = None;
            if let Some(teams) = &repo.teams {
                for (team_name, role) in teams {
                    if let Some(team) = self.directory.get_team(team_name)
                        && (team.maintainers.contains(user_name) || team.members.contains(user_name))
                    {
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
            highest_team_role
        };

        // Check teams' maintainers are members of the organization
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
                format!("{i}")
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
                    if let Some((team_name, highest_team_role)) = highest_team_role
                        && &highest_team_role > user_role
                    {
                        merr.push(format_err!(
                            "repo[{id}]: collaborator {user_name} already has {highest_team_role} \
                            access from team {team_name}"
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
}

/// Repository information.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Repository {
    pub name: String,

    #[serde(alias = "external_collaborators", skip_serializing_if = "Option::is_none")]
    pub collaborators: Option<BTreeMap<UserName, Role>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub teams: Option<BTreeMap<TeamName, Role>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
}

/// Role a user or team may have been assigned.
#[derive(Debug, Clone, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
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
            Some(_) | None => Role::default(),
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
            Some(_) | None => Role::default(),
        }
    }
}

/// Repository visibility.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Internal,
    Private,
    #[default]
    Public,
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Visibility::Internal => write!(f, "internal"),
            Visibility::Private => write!(f, "private"),
            Visibility::Public => write!(f, "public"),
        }
    }
}

impl From<String> for Visibility {
    fn from(value: String) -> Self {
        match value.as_str() {
            "internal" => Visibility::Internal,
            "private" => Visibility::Private,
            "public" => Visibility::Public,
            _ => Visibility::default(),
        }
    }
}

/// Represents the changes between two states.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    pub directory: Vec<DirectoryChange>,
    pub repositories: Vec<RepositoryChange>,
}

/// Represents a repository change.
#[derive(Debug, Clone, PartialEq)]
pub enum RepositoryChange {
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
                if let Some(teams) = &repo.teams
                    && !teams.is_empty()
                {
                    write!(s, "\n\t- Teams")?;
                    for (team_name, role) in teams {
                        write!(s, "\n\t\t- **{team_name}**: *{role}*")?;
                    }
                }
                if let Some(collaborators) = &repo.collaborators
                    && !collaborators.is_empty()
                {
                    write!(s, "\n\t- Collaborators")?;
                    for (user_name, role) in collaborators {
                        write!(s, "\n\t\t- **{user_name}**: *{role}*")?;
                    }
                }
            }
            RepositoryChange::TeamAdded(repo_name, team_name, role) => {
                write!(
                    s,
                    "- team **{team_name}** has been *added* to repository **{repo_name}** (role: **{role}**)"
                )?;
            }
            RepositoryChange::TeamRemoved(repo_name, team_name) => {
                write!(
                    s,
                    "- team **{team_name}** has been *removed* from repository **{repo_name}**"
                )?;
            }
            RepositoryChange::TeamRoleUpdated(repo_name, team_name, role) => {
                write!(
                    s,
                    "- team **{team_name}** role in repository **{repo_name}** has been *updated* to **{role}**"
                )?;
            }
            RepositoryChange::CollaboratorAdded(repo_name, user_name, role) => {
                write!(
                    s,
                    "- user **{user_name}** is now a collaborator (role: **{role}**) of repository **{repo_name}**"
                )?;
            }
            RepositoryChange::CollaboratorRemoved(repo_name, user_name) => {
                write!(
                    s,
                    "- user **{user_name}** is no longer a collaborator of repository **{repo_name}**"
                )?;
            }
            RepositoryChange::CollaboratorRoleUpdated(repo_name, user_name, role) => {
                write!(
                    s,
                    "- user **{user_name}** role in repository **{repo_name}** has been updated to **{role}**"
                )?;
            }
            RepositoryChange::VisibilityUpdated(repo_name, visibility) => {
                write!(
                    s,
                    "- repository **{repo_name}** visibility has been updated to **{visibility}**"
                )?;
            }
        }

        Ok(s)
    }
}

/// Check if the slug provided belongs to an enterprise team. Enterprise teams
/// are managed at the enterprise level and cannot be defined in the
/// configuration, so they are not part of the state.
fn is_enterprise_team(slug: &str) -> bool {
    slug.starts_with(ENTERPRISE_TEAM_SLUG_PREFIX)
}

#[cfg(test)]
mod tests;
