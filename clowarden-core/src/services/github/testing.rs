//! This module provides the fixtures and mock setup helpers shared by the
//! GitHub service handler tests.

use std::future;

use anyhow::{Error, format_err};
use octorust::types::{
    Collaborator, MinimalRepository, OrgMembershipState, OrganizationInvitation, RepositoryInvitation,
    SimpleUser, Team, TeamMembership, TeamMembershipRole,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{
    cfg::{Legacy, Organization},
    github::{MockGH, Source},
    multierror::MultiError,
};

use super::{
    service::{Ctx, MockSvc},
    state::Role,
};

/// Reference of the configuration in the base branch.
pub(crate) const BASE_REF: &str = "main";

/// Path of the sheriff permissions file.
pub(crate) const CONFIG_PATH: &str = "config.yaml";

/// Reference of the configuration in the pull request head.
pub(crate) const HEAD_REF: &str = "head";

/// Organization name.
pub(crate) const ORG: &str = "org";

/// Repository where the configuration lives.
const CONFIG_REPO: &str = ".github";

/// Installation id of the GitHub application.
const INSTALLATION_ID: i64 = 1;

/// Description of a repository in the service.
#[derive(Clone, Default)]
pub(crate) struct ServiceRepository {
    pub name: &'static str,

    /// Whether GitHub reports the repository as archived. Archived
    /// repositories are not inspected when loading the state.
    pub archived: bool,

    pub collaborators: Vec<(&'static str, Role)>,

    /// Whether the state load must leave this repository out without
    /// inspecting it.
    pub ignored: bool,

    pub invitations: Vec<RepositoryInvitation>,
    pub teams: Vec<(&'static str, Role)>,
    pub visibility: &'static str,
}

impl ServiceRepository {
    /// Create a public repository with no collaborators or teams.
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            name,
            visibility: "public",
            ..Default::default()
        }
    }
}

/// Description of the state of an organization in the service.
#[derive(Clone, Default)]
pub(crate) struct ServiceState {
    pub admins: Vec<&'static str>,
    pub members: Vec<&'static str>,
    pub repositories: Vec<ServiceRepository>,
    pub teams: Vec<ServiceTeam>,
}

/// Description of a team in the service.
#[derive(Clone, Default)]
pub(crate) struct ServiceTeam {
    pub slug: &'static str,

    /// Invitations in the team. The membership is the one returned for the
    /// invited user, and a value of none means it must not be requested.
    pub invitations: Vec<(&'static str, Option<TeamMembership>)>,

    pub maintainers: Vec<&'static str>,
    pub members: Vec<&'static str>,
    pub name: &'static str,
}

impl ServiceTeam {
    /// Create a team with the slug and maintainer provided.
    pub(crate) fn new(slug: &'static str, maintainer: &'static str) -> Self {
        Self {
            slug,
            maintainers: vec![maintainer],
            name: slug,
            ..Default::default()
        }
    }
}

/// Expect the configuration file to be read the given number of times.
pub(crate) fn expect_config_file(gh: &mut MockGH, ref_: &'static str, times: usize, content: &str) {
    let content = content.to_string();
    gh.expect_get_file_content()
        .withf(move |src, path| src.owner == ORG && src.ref_ == ref_ && path == CONFIG_PATH)
        .times(times)
        .returning(move |_, _| Box::pin(future::ready(Ok(content.clone()))));
}

/// Expect a single attempt to read the configuration file, which fails.
pub(crate) fn expect_config_file_error(gh: &mut MockGH, ref_: &'static str, message: &'static str) {
    gh.expect_get_file_content()
        .withf(move |src, path| src.owner == ORG && src.ref_ == ref_ && path == CONFIG_PATH)
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Err(format_err!(message)))));
}

/// Expect the reads made to build a state from the configuration.
pub(crate) fn expect_config_load(svc: &mut MockSvc, state: &ServiceState) {
    expect_failed_config_load(svc, state);

    let members: Vec<SimpleUser> = state.members.iter().map(|login| sample_user(login)).collect();
    svc.expect_list_org_members()
        .withf(|ctx| ctx.org == ORG)
        .times(1)
        .returning(move |_| Box::pin(future::ready(Ok(members.clone()))));
}

/// Expect the reads made to build a state from a configuration that turns out
/// to be unavailable or invalid before the state is validated.
pub(crate) fn expect_failed_config_load(svc: &mut MockSvc, state: &ServiceState) {
    expect_org_admins(svc, state);
    expect_repositories(svc, state);
}

/// Expect no mutation to be applied to the service.
pub(crate) fn expect_no_mutations(svc: &mut MockSvc) {
    svc.expect_add_repository().never();
    svc.expect_add_repository_collaborator().never();
    svc.expect_add_repository_team().never();
    svc.expect_add_team().never();
    svc.expect_add_team_maintainer().never();
    svc.expect_add_team_member().never();
    svc.expect_remove_repository_collaborator().never();
    svc.expect_remove_repository_invitation().never();
    svc.expect_remove_repository_team().never();
    svc.expect_remove_team().never();
    svc.expect_remove_team_maintainer().never();
    svc.expect_remove_team_member().never();
    svc.expect_update_repository_collaborator_role().never();
    svc.expect_update_repository_invitation().never();
    svc.expect_update_repository_team_role().never();
    svc.expect_update_repository_visibility().never();
}

/// Expect the reads made to build a state from the service.
pub(crate) fn expect_service_load(svc: &mut MockSvc, state: &ServiceState) {
    // Teams
    let teams: Vec<Team> = state.teams.iter().map(|team| sample_team(team.name, team.slug)).collect();
    svc.expect_list_teams()
        .withf(|ctx| ctx.org == ORG)
        .times(1)
        .returning(move |_| Box::pin(future::ready(Ok(teams.clone()))));
    for team in &state.teams {
        expect_team_load(svc, team);
    }

    // Repositories
    expect_org_admins(svc, state);
    expect_repositories(svc, state);
    for repo in &state.repositories {
        expect_repository_load(svc, repo);
    }
}

/// Return the messages of the errors aggregated in the error provided, sorted
/// so that they can be compared regardless of the validation order.
pub(crate) fn multierror_messages(err: &Error) -> Vec<String> {
    let merr = err.downcast_ref::<MultiError>().expect("error to be a multierror");
    let mut messages: Vec<String> = merr.errors().iter().map(ToString::to_string).collect();
    messages.sort();
    messages
}

/// Build the context used to reach the service.
pub(crate) fn sample_ctx() -> Ctx {
    Ctx::from(&sample_org())
}

/// Build the source of the configuration in the pull request head.
pub(crate) fn sample_head_src() -> Source {
    Source {
        ref_: HEAD_REF.to_string(),
        ..sample_src()
    }
}

/// Build the organization used in the tests.
pub(crate) fn sample_org() -> Organization {
    Organization {
        name: ORG.to_string(),
        installation_id: INSTALLATION_ID,
        repository: CONFIG_REPO.to_string(),
        branch: BASE_REF.to_string(),
        legacy: Legacy {
            enabled: true,
            sheriff_permissions_path: CONFIG_PATH.to_string(),
            cncf_people_path: None,
        },
    }
}

/// Build the cumulative permission flags GitHub returns for the role provided.
pub(crate) fn sample_permissions(role: &Role) -> Value {
    json!({
        "admin": matches!(role, Role::Admin),
        "maintain": matches!(role, Role::Admin | Role::Maintain),
        "pull": true,
        "push": matches!(role, Role::Admin | Role::Maintain | Role::Write),
        "triage": matches!(role, Role::Admin | Role::Maintain | Role::Write | Role::Triage),
    })
}

/// Build a repository as returned when listing the organization's ones.
pub(crate) fn sample_repository(name: &str, visibility: &str, archived: bool) -> MinimalRepository {
    MinimalRepository {
        archived,
        name: name.to_string(),
        visibility: visibility.to_string(),
        ..Default::default()
    }
}

/// Build a repository invitation with the role provided.
pub(crate) fn sample_repository_invitation(
    id: i64,
    invitee: Option<&str>,
    role: &Role,
) -> RepositoryInvitation {
    from_json(json!({
        "id": id,
        "invitee": invitee.map(|login| json!({ "login": login })),
        "permissions": role.to_string(),
        "repository": {},
    }))
}

/// Build the source of the configuration in the base branch.
pub(crate) fn sample_src() -> Source {
    Source::from(&sample_org())
}

/// Build a team membership.
pub(crate) fn sample_team_membership(role: TeamMembershipRole, state: OrgMembershipState) -> TeamMembership {
    TeamMembership {
        role,
        state,
        url: String::new(),
    }
}

/// Build a user with the login provided.
pub(crate) fn sample_user(login: &str) -> SimpleUser {
    from_json(json!({ "login": login }))
}

/// Expect the organization admins to be listed.
fn expect_org_admins(svc: &mut MockSvc, state: &ServiceState) {
    let admins: Vec<SimpleUser> = state.admins.iter().map(|login| sample_user(login)).collect();
    svc.expect_list_org_admins()
        .withf(|ctx| ctx.org == ORG)
        .times(1)
        .returning(move |_| Box::pin(future::ready(Ok(admins.clone()))));
}

/// Expect the organization repositories to be listed.
fn expect_repositories(svc: &mut MockSvc, state: &ServiceState) {
    let repositories: Vec<MinimalRepository> = state
        .repositories
        .iter()
        .map(|repo| sample_repository(repo.name, repo.visibility, repo.archived))
        .collect();
    svc.expect_list_repositories()
        .withf(|ctx| ctx.org == ORG)
        .times(1)
        .returning(move |_| Box::pin(future::ready(Ok(repositories.clone()))));
}

/// Expect the details of the repository provided to be read, unless it is one
/// the state load has to leave out.
fn expect_repository_load(svc: &mut MockSvc, repo: &ServiceRepository) {
    let name = repo.name;

    if repo.archived || repo.ignored {
        svc.expect_list_repository_collaborators()
            .withf(move |_, repo_name| repo_name == name)
            .never();
        svc.expect_list_repository_invitations()
            .withf(move |_, repo_name| repo_name == name)
            .never();
        svc.expect_list_repository_teams().withf(move |_, repo_name| repo_name == name).never();
        return;
    }

    let collaborators: Vec<Collaborator> =
        repo.collaborators.iter().map(|(login, role)| sample_collaborator(login, role)).collect();
    svc.expect_list_repository_collaborators()
        .withf(move |ctx, repo_name| ctx.org == ORG && repo_name == name)
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Ok(collaborators.clone()))));

    let invitations = repo.invitations.clone();
    svc.expect_list_repository_invitations()
        .withf(move |ctx, repo_name| ctx.org == ORG && repo_name == name)
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Ok(invitations.clone()))));

    let teams: Vec<Team> = repo.teams.iter().map(|(slug, role)| sample_repository_team(slug, role)).collect();
    svc.expect_list_repository_teams()
        .withf(move |ctx, repo_name| ctx.org == ORG && repo_name == name)
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Ok(teams.clone()))));
}

/// Expect the details of the team provided to be read.
fn expect_team_load(svc: &mut MockSvc, team: &ServiceTeam) {
    let slug = team.slug;

    let maintainers: Vec<SimpleUser> = team.maintainers.iter().map(|login| sample_user(login)).collect();
    svc.expect_list_team_maintainers()
        .withf(move |ctx, team_name| ctx.org == ORG && team_name == slug)
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Ok(maintainers.clone()))));

    let members: Vec<SimpleUser> = team.members.iter().map(|login| sample_user(login)).collect();
    svc.expect_list_team_members()
        .withf(move |ctx, team_name| ctx.org == ORG && team_name == slug)
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Ok(members.clone()))));

    let invitations: Vec<OrganizationInvitation> =
        team.invitations.iter().map(|(login, _)| sample_org_invitation(login)).collect();
    svc.expect_list_team_invitations()
        .withf(move |ctx, team_name| ctx.org == ORG && team_name == slug)
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Ok(invitations.clone()))));

    for (login, membership) in team.invitations.clone() {
        let expectation = svc.expect_get_team_membership().withf(move |ctx, team_name, user_name| {
            ctx.org == ORG && team_name == slug && user_name == login
        });
        match membership {
            Some(membership) => {
                expectation
                    .times(1)
                    .returning(move |_, _, _| Box::pin(future::ready(Ok(membership.clone()))));
            }
            None => {
                expectation.never();
            }
        }
    }
}

/// Deserialize a fixture from the JSON value provided.
fn from_json<T: DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value).expect("fixture to be valid")
}

/// Build a collaborator with the role provided.
fn sample_collaborator(login: &str, role: &Role) -> Collaborator {
    from_json(json!({ "login": login, "permissions": sample_permissions(role) }))
}

/// Build an organization invitation for the login provided.
fn sample_org_invitation(login: &str) -> OrganizationInvitation {
    from_json(json!({ "login": login }))
}

/// Build a team as returned when listing the ones of a repository. Its name
/// differs from its slug, as the slug is what identifies a team.
fn sample_repository_team(slug: &str, role: &Role) -> Team {
    from_json(json!({
        "name": slug.to_uppercase(),
        "permissions": sample_permissions(role),
        "slug": slug,
    }))
}

/// Build a team as returned when listing the organization's ones.
fn sample_team(name: &str, slug: &str) -> Team {
    from_json(json!({ "name": name, "slug": slug }))
}
