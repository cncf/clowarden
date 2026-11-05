use std::{collections::BTreeMap, sync::Arc};

use serde_json::json;

use crate::{
    cfg::GitHubApp,
    directory::Team,
    github::http_fake::{FakeGitHub, Request, Route, generate_app_private_key},
    services::github::state::{Repository, Role, Visibility},
};

use super::{Ctx, Svc, SvcApi};

const COLLABORATOR_PATH: &str = "/repos/org1/repo1/collaborators/user1";
const INSTALLATION_ID: i64 = 1;
const INVITATION_ID: i64 = 7;
const INVITATION_PATH: &str = "/repos/org1/repo1/invitations/7";
const MEMBERSHIP_PATH: &str = "/orgs/org1/teams/team1/memberships/user1";
const ORG: &str = "org1";
const REPO: &str = "repo1";
const REPO_TEAMS_PATH: &str = "/repos/org1/repo1/teams";
const REPOS_PATH: &str = "/orgs/org1/repos";
const TEAM: &str = "team1";
const TEAM_INVITATIONS_PATH: &str = "/orgs/org1/teams/team1/invitations";
const TEAM_PATH: &str = "/orgs/org1/teams/team1";
const TEAM_REPO_PATH: &str = "/orgs/org1/teams/team1/repos/org1/repo1";
const TEAMS_PATH: &str = "/orgs/org1/teams";
const UNPROCESSABLE_ERROR: &str = "HTTP Error. Code: 422 Unprocessable Entity, message: validation failed";
const USER: &str = "user1";
const USER_PATH: &str = "/users/user1";

#[tokio::test]
async fn test_add_repository_collaborator_sends_read_permission() {
    // Setup fake answering with the invitation created
    let body = r#"{"repository":{}}"#;
    let fake = FakeGitHub::start(vec![Route::new("PUT", COLLABORATOR_PATH, 201, body)]);
    let svc = setup_svc(&fake);

    // Add the collaborator
    let result = svc
        .add_repository_collaborator(&ctx(ORG), &REPO.to_string(), &USER.to_string(), &Role::Read)
        .await;

    // Check the request sent
    assert!(result.is_ok());
    let body = json!({ "permission": "pull" });
    assert_eq!(
        fake.requests(),
        vec![Request::new("PUT", COLLABORATOR_PATH, Some(body))]
    );
}

#[tokio::test]
async fn test_add_repository_grants_team_and_collaborator_access() {
    // Setup fake accepting the repository and its access grants
    let fake = FakeGitHub::start(vec![
        Route::new("POST", REPOS_PATH, 201, "{}"),
        Route::new("PUT", TEAM_REPO_PATH, 204, ""),
        Route::new("PUT", COLLABORATOR_PATH, 201, r#"{"repository":{}}"#),
    ]);
    let svc = setup_svc(&fake);

    // Add the repository
    let result = svc.add_repository(&ctx(ORG), &sample_repository()).await;

    // Check the team and the collaborator were granted access
    assert!(result.is_ok());
    let requests = fake.requests();
    let team_body = json!({ "permission": "push" });
    let collaborator_body = json!({ "permission": "pull" });
    assert!(requests.contains(&Request::new("PUT", TEAM_REPO_PATH, Some(team_body))));
    assert!(requests.contains(&Request::new("PUT", COLLABORATOR_PATH, Some(collaborator_body))));
}

#[tokio::test]
async fn test_add_repository_sends_only_provided_fields() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("POST", REPOS_PATH, 201, "{}")]);
    let svc = setup_svc(&fake);

    for (visibility, expected_visibility) in [
        (Visibility::Internal, "internal"),
        (Visibility::Private, "private"),
        (Visibility::Public, "public"),
    ] {
        // Add the repository
        let repo = Repository {
            name: REPO.to_string(),
            visibility: Some(visibility),
            ..Default::default()
        };
        let result = svc.add_repository(&ctx(ORG), &repo).await;

        // Check the body does not include any empty fields
        assert!(result.is_ok());
        let body = json!({ "name": REPO, "visibility": expected_visibility });
        assert_eq!(
            fake.requests().last(),
            Some(&Request::new("POST", REPOS_PATH, Some(body)))
        );
    }
}

#[tokio::test]
async fn test_add_repository_stops_when_creation_fails() {
    // Setup fake rejecting the repository creation
    let fake = FakeGitHub::start(vec![Route::new("POST", REPOS_PATH, 422, "validation failed")]);
    let svc = setup_svc(&fake);

    // Add the repository
    let err = svc.add_repository(&ctx(ORG), &sample_repository()).await.unwrap_err();

    // Check the error is returned and no access was granted
    assert_eq!(err.to_string(), UNPROCESSABLE_ERROR);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, REPOS_PATH);
}

#[tokio::test]
async fn test_add_repository_stops_when_team_grant_fails() {
    // Setup fake rejecting the team access grant
    let fake = FakeGitHub::start(vec![
        Route::new("POST", REPOS_PATH, 201, "{}"),
        Route::new("PUT", TEAM_REPO_PATH, 422, "validation failed"),
    ]);
    let svc = setup_svc(&fake);

    // Add the repository
    let err = svc.add_repository(&ctx(ORG), &sample_repository()).await.unwrap_err();

    // Check the error is returned and the collaborator was not added
    assert_eq!(err.to_string(), UNPROCESSABLE_ERROR);
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, REPOS_PATH);
    assert_eq!(requests[1].path, TEAM_REPO_PATH);
}

#[tokio::test]
async fn test_add_repository_team_sends_write_permission() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("PUT", TEAM_REPO_PATH, 204, "")]);
    let svc = setup_svc(&fake);

    // Add the team to the repository
    let result = svc
        .add_repository_team(&ctx(ORG), &REPO.to_string(), &TEAM.to_string(), &Role::Write)
        .await;

    // Check the request sent
    assert!(result.is_ok());
    let body = json!({ "permission": "push" });
    assert_eq!(
        fake.requests(),
        vec![Request::new("PUT", TEAM_REPO_PATH, Some(body))]
    );
}

#[tokio::test]
async fn test_add_team_creates_team_and_adds_members() {
    // Setup fake and team
    let member_path = "/orgs/org1/teams/team1/memberships/user2";
    let fake = FakeGitHub::start(vec![
        Route::new("POST", TEAMS_PATH, 201, r#"{"organization":{}}"#),
        Route::new("PUT", member_path, 200, "{}"),
    ]);
    let svc = setup_svc(&fake);
    let team = Team {
        name: TEAM.to_string(),
        maintainers: vec![USER.to_string()],
        members: vec!["user2".to_string()],
        ..Default::default()
    };

    // Add the team
    let result = svc.add_team(&ctx(ORG), &team).await;

    // Check the creation body does not include any empty fields
    assert!(result.is_ok());
    let team_body = json!({ "maintainers": [USER], "name": TEAM, "privacy": "closed" });
    let member_body = json!({ "role": "member" });
    assert_eq!(
        fake.requests(),
        vec![
            Request::new("POST", TEAMS_PATH, Some(team_body)),
            Request::new("PUT", member_path, Some(member_body)),
        ]
    );
}

#[tokio::test]
async fn test_add_team_maintainer_sends_maintainer_role() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("PUT", MEMBERSHIP_PATH, 200, "{}")]);
    let svc = setup_svc(&fake);

    // Add the maintainer
    let result = svc.add_team_maintainer(&ctx(ORG), &TEAM.to_string(), &USER.to_string()).await;

    // Check the request sent
    assert!(result.is_ok());
    let body = json!({ "role": "maintainer" });
    assert_eq!(
        fake.requests(),
        vec![Request::new("PUT", MEMBERSHIP_PATH, Some(body))]
    );
}

#[tokio::test]
async fn test_add_team_member_sends_member_role() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("PUT", MEMBERSHIP_PATH, 200, "{}")]);
    let svc = setup_svc(&fake);

    // Add the member
    let result = svc.add_team_member(&ctx(ORG), &TEAM.to_string(), &USER.to_string()).await;

    // Check the request sent
    assert!(result.is_ok());
    let body = json!({ "role": "member" });
    assert_eq!(
        fake.requests(),
        vec![Request::new("PUT", MEMBERSHIP_PATH, Some(body))]
    );
}

#[tokio::test]
async fn test_add_team_stops_when_creation_fails() {
    // Setup fake rejecting the team creation
    let fake = FakeGitHub::start(vec![Route::new("POST", TEAMS_PATH, 422, "validation failed")]);
    let svc = setup_svc(&fake);
    let team = Team {
        name: TEAM.to_string(),
        maintainers: vec![USER.to_string()],
        members: vec!["user2".to_string()],
        ..Default::default()
    };

    // Add the team
    let err = svc.add_team(&ctx(ORG), &team).await.unwrap_err();

    // Check the error is returned and no members were added
    assert_eq!(err.to_string(), UNPROCESSABLE_ERROR);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, TEAMS_PATH);
}

#[tokio::test]
async fn test_get_user_login_returns_canonical_case() {
    // Setup fake answering with the login as GitHub has it
    let fake = FakeGitHub::start(vec![Route::new("GET", USER_PATH, 200, r#"{"login":"User1"}"#)]);
    let svc = setup_svc(&fake);

    // Get the user login
    let login = svc.get_user_login(&ctx(ORG), &USER.to_string()).await.unwrap();

    // Check the login returned and the request sent
    assert_eq!(login, "User1");
    assert_eq!(fake.requests(), vec![Request::new("GET", USER_PATH, None)]);
}

#[tokio::test]
async fn test_list_org_admins_requests_admin_role() {
    // Setup fake (the org is unique because this wrapper is cached by org)
    let path = "/orgs/list-org-admins/members";
    let fake = FakeGitHub::start(vec![Route::new("GET", path, 200, r#"[{"login":"user1"}]"#)]);
    let svc = setup_svc(&fake);

    // List the admins
    let admins = svc.list_org_admins(&ctx("list-org-admins")).await.unwrap();

    // Check the users returned and the filters sent
    assert_eq!(names(admins.iter().map(|u| &u.login)), vec![USER]);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, path);
    assert_eq!(requests[0].query_param("filter"), Some("all"));
    assert_eq!(requests[0].query_param("role"), Some("admin"));
}

#[tokio::test]
async fn test_list_org_members_requests_all_roles() {
    // Setup fake (the org is unique because this wrapper is cached by org)
    let path = "/orgs/list-org-members/members";
    let fake = FakeGitHub::start(vec![Route::new("GET", path, 200, r#"[{"login":"user1"}]"#)]);
    let svc = setup_svc(&fake);

    // List the members
    let members = svc.list_org_members(&ctx("list-org-members")).await.unwrap();

    // Check the users returned and the filters sent
    assert_eq!(names(members.iter().map(|u| &u.login)), vec![USER]);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, path);
    assert_eq!(requests[0].query_param("filter"), Some("all"));
    assert_eq!(requests[0].query_param("role"), Some("all"));
}

#[tokio::test]
async fn test_list_repositories_requests_all_types() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("GET", REPOS_PATH, 200, r#"[{"name":"repo1"}]"#)]);
    let svc = setup_svc(&fake);

    // List the repositories
    let repos = svc.list_repositories(&ctx(ORG)).await.unwrap();

    // Check the repositories returned and the filter sent
    assert_eq!(names(repos.iter().map(|r| &r.name)), vec![REPO]);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].query_param("type"), Some("all"));
}

#[tokio::test]
async fn test_list_repository_collaborators_requests_direct_affiliation() {
    // Setup fake
    let path = "/repos/org1/repo1/collaborators";
    let fake = FakeGitHub::start(vec![Route::new("GET", path, 200, r#"[{"login":"user1"}]"#)]);
    let svc = setup_svc(&fake);

    // List the collaborators
    let collaborators = svc.list_repository_collaborators(&ctx(ORG), &REPO.to_string()).await.unwrap();

    // Check the users returned and the filter sent
    assert_eq!(names(collaborators.iter().map(|c| &c.login)), vec![USER]);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].query_param("affiliation"), Some("direct"));
}

#[tokio::test]
async fn test_list_repository_invitations_is_isolated_per_organization() {
    // Setup fake answering with a different invitation per organization
    let path_a = "/repos/invitations-org-a/shared-repo/invitations";
    let path_b = "/repos/invitations-org-b/shared-repo/invitations";
    let fake = FakeGitHub::start(vec![
        Route::new("GET", path_a, 200, r#"[{"id":1,"repository":{}}]"#),
        Route::new("GET", path_b, 200, r#"[{"id":2,"repository":{}}]"#),
    ]);
    let svc = setup_svc(&fake);
    let repo_name = "shared-repo".to_string();

    // List the invitations of the repository in both organizations
    let invitations_a = svc.list_repository_invitations(&ctx("invitations-org-a"), &repo_name).await.unwrap();
    let invitations_b = svc.list_repository_invitations(&ctx("invitations-org-b"), &repo_name).await.unwrap();

    // Check each organization got its own invitations
    assert_eq!(invitations_a.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1]);
    assert_eq!(invitations_b.iter().map(|i| i.id).collect::<Vec<_>>(), vec![2]);
    assert_eq!(fake.requests().len(), 2);
}

#[tokio::test]
async fn test_list_repository_teams_requests_repository_teams() {
    // Setup fake
    let body = r#"[{"slug":"team1"}]"#;
    let fake = FakeGitHub::start(vec![Route::new("GET", REPO_TEAMS_PATH, 200, body)]);
    let svc = setup_svc(&fake);

    // List the repository teams
    let teams = svc.list_repository_teams(&ctx(ORG), &REPO.to_string()).await.unwrap();

    // Check the teams returned and the request sent
    assert_eq!(names(teams.iter().map(|t| &t.slug)), vec![TEAM]);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, REPO_TEAMS_PATH);
}

#[tokio::test]
async fn test_list_team_invitations_requests_team_invitations() {
    // Setup fake
    let body = r#"[{"login":"user1"}]"#;
    let fake = FakeGitHub::start(vec![Route::new("GET", TEAM_INVITATIONS_PATH, 200, body)]);
    let svc = setup_svc(&fake);

    // List the team invitations
    let invitations = svc.list_team_invitations(&ctx(ORG), &TEAM.to_string()).await.unwrap();

    // Check the invitations returned and the request sent
    assert_eq!(names(invitations.iter().map(|i| &i.login)), vec![USER]);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, TEAM_INVITATIONS_PATH);
}

#[tokio::test]
async fn test_list_team_maintainers_requests_maintainer_role() {
    // Setup fake
    let path = "/orgs/org1/teams/team1/members";
    let fake = FakeGitHub::start(vec![Route::new("GET", path, 200, r#"[{"login":"user1"}]"#)]);
    let svc = setup_svc(&fake);

    // List the maintainers
    let maintainers = svc.list_team_maintainers(&ctx(ORG), &TEAM.to_string()).await.unwrap();

    // Check the users returned and the filter sent
    assert_eq!(names(maintainers.iter().map(|u| &u.login)), vec![USER]);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].query_param("role"), Some("maintainer"));
}

#[tokio::test]
async fn test_list_team_members_requests_member_role() {
    // Setup fake
    let path = "/orgs/org1/teams/team1/members";
    let fake = FakeGitHub::start(vec![Route::new("GET", path, 200, r#"[{"login":"user1"}]"#)]);
    let svc = setup_svc(&fake);

    // List the members
    let members = svc.list_team_members(&ctx(ORG), &TEAM.to_string()).await.unwrap();

    // Check the users returned and the filter sent
    assert_eq!(names(members.iter().map(|u| &u.login)), vec![USER]);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].query_param("role"), Some("member"));
}

#[tokio::test]
async fn test_list_teams_requests_organization_teams() {
    // Setup fake
    let body = r#"[{"slug":"team1"}]"#;
    let fake = FakeGitHub::start(vec![Route::new("GET", TEAMS_PATH, 200, body)]);
    let svc = setup_svc(&fake);

    // List the teams
    let teams = svc.list_teams(&ctx(ORG)).await.unwrap();

    // Check the teams returned and the request sent
    assert_eq!(names(teams.iter().map(|t| &t.slug)), vec![TEAM]);
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, TEAMS_PATH);
}

#[test]
fn test_new_with_app_creds_allows_getting_installation_client() {
    // Build the instance
    let svc = SvcApi::new_with_app_creds(&sample_github_app(&generate_app_private_key())).unwrap();

    // Check a client is available for the installation
    assert!(svc.get_client(Some(INSTALLATION_ID)).is_ok());
}

#[test]
fn test_new_with_app_creds_rejects_malformed_pem() {
    // Build the instance
    let result = SvcApi::new_with_app_creds(&sample_github_app("not a pem"));

    // Check the parsing error is returned
    assert!(result.err().unwrap().downcast_ref::<pem::PemError>().is_some());
}

#[test]
fn test_new_with_token_allows_getting_client_without_installation() {
    // Build the instance
    let svc = SvcApi::new_with_token("token".to_string());

    // Check a client is available when no installation is involved
    assert!(svc.get_client(None).is_ok());
}

#[tokio::test]
async fn test_remove_repository_collaborator_deletes_collaborator() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("DELETE", COLLABORATOR_PATH, 204, "")]);
    let svc = setup_svc(&fake);

    // Remove the collaborator
    let result = svc.remove_repository_collaborator(&ctx(ORG), &REPO.to_string(), &USER.to_string()).await;

    // Check the request sent
    assert!(result.is_ok());
    assert_eq!(
        fake.requests(),
        vec![Request::new("DELETE", COLLABORATOR_PATH, None)]
    );
}

#[tokio::test]
async fn test_remove_repository_invitation_deletes_invitation() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("DELETE", INVITATION_PATH, 204, "")]);
    let svc = setup_svc(&fake);

    // Remove the invitation
    let result = svc.remove_repository_invitation(&ctx(ORG), &REPO.to_string(), INVITATION_ID).await;

    // Check the request sent
    assert!(result.is_ok());
    assert_eq!(
        fake.requests(),
        vec![Request::new("DELETE", INVITATION_PATH, None)]
    );
}

#[tokio::test]
async fn test_remove_repository_team_deletes_team_access() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("DELETE", TEAM_REPO_PATH, 204, "")]);
    let svc = setup_svc(&fake);

    // Remove the team from the repository
    let result = svc.remove_repository_team(&ctx(ORG), &REPO.to_string(), &TEAM.to_string()).await;

    // Check the request sent
    assert!(result.is_ok());
    assert_eq!(
        fake.requests(),
        vec![Request::new("DELETE", TEAM_REPO_PATH, None)]
    );
}

#[tokio::test]
async fn test_remove_team_deletes_team() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("DELETE", TEAM_PATH, 204, "")]);
    let svc = setup_svc(&fake);

    // Remove the team
    let result = svc.remove_team(&ctx(ORG), &TEAM.to_string()).await;

    // Check the request sent
    assert!(result.is_ok());
    assert_eq!(fake.requests(), vec![Request::new("DELETE", TEAM_PATH, None)]);
}

#[tokio::test]
async fn test_remove_team_maintainer_deletes_membership() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("DELETE", MEMBERSHIP_PATH, 204, "")]);
    let svc = setup_svc(&fake);

    // Remove the maintainer
    let result = svc.remove_team_maintainer(&ctx(ORG), &TEAM.to_string(), &USER.to_string()).await;

    // Check the request sent
    assert!(result.is_ok());
    assert_eq!(
        fake.requests(),
        vec![Request::new("DELETE", MEMBERSHIP_PATH, None)]
    );
}

#[tokio::test]
async fn test_remove_team_member_deletes_membership() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("DELETE", MEMBERSHIP_PATH, 204, "")]);
    let svc = setup_svc(&fake);

    // Remove the member
    let result = svc.remove_team_member(&ctx(ORG), &TEAM.to_string(), &USER.to_string()).await;

    // Check the request sent
    assert!(result.is_ok());
    assert_eq!(
        fake.requests(),
        vec![Request::new("DELETE", MEMBERSHIP_PATH, None)]
    );
}

#[tokio::test]
async fn test_update_repository_collaborator_role_accepts_empty_response() {
    // Setup fake answering as GitHub does for existing collaborators
    let fake = FakeGitHub::start(vec![Route::new("PUT", COLLABORATOR_PATH, 204, "")]);
    let svc = setup_svc(&fake);

    // Update the collaborator role
    let result = svc
        .update_repository_collaborator_role(&ctx(ORG), &REPO.to_string(), &USER.to_string(), &Role::Read)
        .await;

    // Check the request sent
    assert!(result.is_ok());
    let body = json!({ "permission": "pull" });
    assert_eq!(
        fake.requests(),
        vec![Request::new("PUT", COLLABORATOR_PATH, Some(body))]
    );
}

#[tokio::test]
async fn test_update_repository_invitation_sends_write_permissions() {
    // Setup fake
    let body = r#"{"repository":{}}"#;
    let fake = FakeGitHub::start(vec![Route::new("PATCH", INVITATION_PATH, 200, body)]);
    let svc = setup_svc(&fake);

    // Update the invitation
    let result = svc
        .update_repository_invitation(&ctx(ORG), &REPO.to_string(), INVITATION_ID, &Role::Write)
        .await;

    // Check the request sent
    assert!(result.is_ok());
    let body = json!({ "permissions": "write" });
    assert_eq!(
        fake.requests(),
        vec![Request::new("PATCH", INVITATION_PATH, Some(body))]
    );
}

#[tokio::test]
async fn test_update_repository_team_role_sends_write_permission() {
    // Setup fake
    let fake = FakeGitHub::start(vec![Route::new("PUT", TEAM_REPO_PATH, 204, "")]);
    let svc = setup_svc(&fake);

    // Update the team role
    let result = svc
        .update_repository_team_role(&ctx(ORG), &REPO.to_string(), &TEAM.to_string(), &Role::Write)
        .await;

    // Check the request sent
    assert!(result.is_ok());
    let body = json!({ "permission": "push" });
    assert_eq!(
        fake.requests(),
        vec![Request::new("PUT", TEAM_REPO_PATH, Some(body))]
    );
}

#[tokio::test]
async fn test_update_repository_visibility_sends_only_name_and_visibility() {
    // Setup fake
    let path = "/repos/org1/repo1";
    let fake = FakeGitHub::start(vec![Route::new("PATCH", path, 200, "{}")]);
    let svc = setup_svc(&fake);

    for (visibility, expected_visibility) in [
        (Visibility::Internal, "internal"),
        (Visibility::Private, "private"),
        (Visibility::Public, "public"),
    ] {
        // Update the visibility
        let result = svc.update_repository_visibility(&ctx(ORG), &REPO.to_string(), &visibility).await;

        // Check the body does not include any empty fields
        assert!(result.is_ok());
        let body = json!({ "name": REPO, "visibility": expected_visibility });
        assert_eq!(
            fake.requests().last(),
            Some(&Request::new("PATCH", path, Some(body)))
        );
    }
}

// Helpers.

/// Create a context targeting the organization provided.
fn ctx(org: &str) -> Ctx {
    Ctx {
        inst_id: None,
        org: org.to_string(),
    }
}

/// Collect the names provided.
fn names<'a>(names: impl Iterator<Item = &'a String>) -> Vec<&'a str> {
    names.map(String::as_str).collect()
}

/// Return a GitHub application using the private key provided.
fn sample_github_app(private_key: &str) -> GitHubApp {
    GitHubApp {
        app_id: 1,
        private_key: private_key.to_string(),
        ..Default::default()
    }
}

/// Return a repository with a team and a collaborator.
fn sample_repository() -> Repository {
    Repository {
        name: REPO.to_string(),
        collaborators: Some(BTreeMap::from([(USER.to_string(), Role::Read)])),
        teams: Some(BTreeMap::from([(TEAM.to_string(), Role::Write)])),
        ..Default::default()
    }
}

/// Setup a SvcApi instance that sends all requests to the fake provided.
fn setup_svc(fake: &FakeGitHub) -> SvcApi {
    let svc = SvcApi::default();
    svc.clients.write().unwrap().insert(None, Arc::new(fake.client()));
    svc
}
