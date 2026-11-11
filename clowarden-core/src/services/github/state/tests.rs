use std::{collections::BTreeMap, future, sync::Arc};

use anyhow::{Result, format_err};
use octorust::types::{
    OrgMembershipState, RepositoryInvitationPermissions, RepositoryPermissions, TeamMembershipRole,
    TeamPermissions, TeamsAddUpdateRepoPermissionsInOrgRequestPermission,
};
use serde_json::json;

use crate::{
    directory::{Directory, Team, User},
    github::MockGH,
    services::{
        Change,
        github::{
            service::MockSvc,
            testing::{
                BASE_REF, ORG, ServiceRepository, ServiceState, ServiceTeam, expect_config_file,
                expect_config_load, expect_service_load, multierror_messages, sample_ctx, sample_org,
                sample_permissions, sample_repository, sample_repository_invitation, sample_src,
                sample_team_membership, sample_user,
            },
        },
    },
};

use super::{Changes, Repository, RepositoryChange, Role, State, Visibility};

#[test]
fn test_diff_detects_multiple_changes() {
    // Setup the old and new states
    let repo1 = Repository {
        name: "repo1".to_string(),
        teams: Some(BTreeMap::from([
            ("team1".to_string(), Role::Write),
            ("team2".to_string(), Role::Write),
        ])),
        visibility: Some(Visibility::Private),
        ..Default::default()
    };
    let repo1_updated = Repository {
        teams: Some(BTreeMap::from([
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

    // Check the changes detected
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

#[test]
fn test_diff_detects_repository_added() {
    // Setup the old and new states
    let repo1 = Repository {
        name: "repo1".to_string(),
        ..Default::default()
    };
    let state1 = State::default();
    let state2 = State {
        repositories: vec![repo1.clone()],
        ..Default::default()
    };

    // Check the changes detected
    assert_eq!(
        state1.diff(&state2),
        Changes {
            repositories: vec![RepositoryChange::RepositoryAdded(repo1)],
            ..Default::default()
        }
    );
}

#[test]
fn test_diff_detects_repository_collaborator_added() {
    // Setup the old and new states
    let repo1 = Repository {
        name: "repo1".to_string(),
        ..Default::default()
    };
    let repo1_adding_collaborator = Repository {
        collaborators: Some(BTreeMap::from([("user1".to_string(), Role::Write)])),
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

    // Check the changes detected
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
fn test_diff_detects_repository_collaborator_removed() {
    // Setup the old and new states
    let repo1 = Repository {
        name: "repo1".to_string(),
        collaborators: Some(BTreeMap::from([("user1".to_string(), Role::Write)])),
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

    // Check the changes detected
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
fn test_diff_detects_repository_collaborator_role_updated() {
    // Setup the old and new states
    let repo1 = Repository {
        name: "repo1".to_string(),
        collaborators: Some(BTreeMap::from([("user1".to_string(), Role::Write)])),
        ..Default::default()
    };
    let repo1_updating_collaborator_role = Repository {
        collaborators: Some(BTreeMap::from([("user1".to_string(), Role::Read)])),
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

    // Check the changes detected
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
fn test_diff_detects_repository_team_added() {
    // Setup the old and new states
    let repo1 = Repository {
        name: "repo1".to_string(),
        ..Default::default()
    };
    let repo1_adding_team = Repository {
        teams: Some(BTreeMap::from([("team1".to_string(), Role::Write)])),
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

    // Check the changes detected
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
fn test_diff_detects_repository_team_removed() {
    // Setup the old and new states
    let repo1 = Repository {
        name: "repo1".to_string(),
        teams: Some(BTreeMap::from([("team1".to_string(), Role::Write)])),
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

    // Check the changes detected
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
fn test_diff_detects_repository_team_role_updated() {
    // Setup the old and new states
    let repo1 = Repository {
        name: "repo1".to_string(),
        teams: Some(BTreeMap::from([("team1".to_string(), Role::Write)])),
        ..Default::default()
    };
    let repo1_updating_team_role = Repository {
        teams: Some(BTreeMap::from([("team1".to_string(), Role::Read)])),
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

    // Check the changes detected
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
fn test_diff_detects_repository_visibility_updated() {
    // Setup the old and new states
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

    // Check the changes detected
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
fn test_diff_discards_user_added() {
    // Setup the old and new states
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

    // Check no changes were detected
    assert_eq!(state1.diff(&state2), Changes::default());
}

#[test]
fn test_diff_ignores_empty_collaborators_and_teams() {
    // Setup the same repository with its maps unset and empty
    let repo1 = Repository {
        name: "repo1".to_string(),
        ..Default::default()
    };
    let repo1_with_empty_maps = Repository {
        collaborators: Some(BTreeMap::new()),
        teams: Some(BTreeMap::new()),
        ..repo1.clone()
    };
    let state1 = State {
        repositories: vec![repo1],
        ..Default::default()
    };
    let state2 = State {
        repositories: vec![repo1_with_empty_maps],
        ..Default::default()
    };

    // Check no changes were detected in any direction
    assert_eq!(state1.diff(&state2), Changes::default());
    assert_eq!(state2.diff(&state1), Changes::default());
}

#[test]
fn test_diff_ignores_repositories_absent_from_new_state() {
    // Setup a repository that is not part of the new state
    let state1 = State {
        repositories: vec![Repository {
            name: "repo1".to_string(),
            collaborators: Some(BTreeMap::from([("user1".to_string(), Role::Write)])),
            teams: Some(BTreeMap::from([("team1".to_string(), Role::Write)])),
            visibility: Some(Visibility::Private),
        }],
        ..Default::default()
    };
    let state2 = State::default();

    // Check no changes were detected
    assert_eq!(state1.diff(&state2), Changes::default());
}

#[tokio::test]
async fn test_new_from_config_defaults_omitted_visibility_to_public() {
    // Setup a configuration that does not define the repository visibility
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        2,
        "
teams: []
repositories:
  - name: repo1
",
    );

    // Setup service
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            visibility: "private",
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_config_load(&mut svc, &service_state);

    // Build the state
    let state = new_from_config(gh, svc).await.unwrap();

    // Check the visibility was set to public
    assert_eq!(state.repositories[0].visibility, Some(Visibility::Public));

    // Check a private repository omitting it would be made public
    let private_repo = Repository {
        name: "repo1".to_string(),
        visibility: Some(Visibility::Private),
        ..Default::default()
    };
    let actual_state = State {
        repositories: vec![private_repo],
        ..Default::default()
    };
    assert_eq!(
        actual_state.diff(&state).repositories,
        vec![RepositoryChange::VisibilityUpdated(
            "repo1".to_string(),
            Visibility::Public
        )]
    );
}

#[tokio::test]
async fn test_new_from_config_fails_when_legacy_format_is_disabled() {
    // Setup a configuration that must not be read
    let mut gh = MockGH::new();
    gh.expect_get_file_content().never();

    // Setup a service that must not be queried
    let mut svc = MockSvc::new();
    svc.expect_list_org_admins().never();
    svc.expect_list_repositories().never();

    // Build the state with the legacy format disabled
    let mut legacy = sample_org().legacy;
    legacy.enabled = false;
    let err = State::new_from_config(Arc::new(gh), Arc::new(svc), &legacy, &sample_ctx(), &sample_src())
        .await
        .unwrap_err();

    // Check the configuration format was reported as unsupported
    assert_eq!(
        err.to_string(),
        "only configuration in legacy format supported at the moment"
    );
}

#[tokio::test]
async fn test_new_from_config_ignores_archived_repositories() {
    // Setup a configuration listing an archived repository
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        2,
        "
teams: []
repositories:
  - name: archived-repo
  - name: repo1
",
    );

    // Setup service
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![
            ServiceRepository {
                archived: true,
                ..ServiceRepository::new("archived-repo")
            },
            ServiceRepository::new("repo1"),
        ],
        ..Default::default()
    };
    expect_config_load(&mut svc, &service_state);

    // Build the state
    let state = new_from_config(gh, svc).await.unwrap();

    // Check only the repository that is not archived was kept
    assert_eq!(repository_names(&state), vec!["repo1"]);
}

#[tokio::test]
async fn test_new_from_config_moves_org_admin_members_to_maintainers() {
    // Setup a configuration listing an org admin as a team member
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        2,
        "
teams:
  - name: team1
    maintainers:
      - maintainer1
    members:
      - admin1
      - member1
repositories: []
",
    );

    // Setup service
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        admins: vec!["admin1"],
        members: vec!["admin1", "maintainer1"],
        ..Default::default()
    };
    expect_config_load(&mut svc, &service_state);

    // Build the state
    let state = new_from_config(gh, svc).await.unwrap();

    // Check the admin was moved and the regular member was left untouched
    let team = &state.directory.teams[0];
    assert_eq!(team.maintainers, vec!["maintainer1", "admin1"]);
    assert_eq!(team.members, vec!["member1"]);
}

#[tokio::test]
async fn test_new_from_config_removes_org_admins_from_collaborators() {
    // Setup a configuration listing an org admin as a collaborator
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        2,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      admin1: admin
      user1: write
  - name: repo2
    external_collaborators:
      admin1: admin
",
    );

    // Setup service
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        admins: vec!["admin1"],
        repositories: vec![ServiceRepository::new("repo1"), ServiceRepository::new("repo2")],
        ..Default::default()
    };
    expect_config_load(&mut svc, &service_state);

    // Build the state
    let state = new_from_config(gh, svc).await.unwrap();

    // Check only the admin was removed
    assert_eq!(
        state.repositories[0].collaborators,
        Some(BTreeMap::from([("user1".to_string(), Role::Write)]))
    );
    assert_eq!(state.repositories[1].collaborators, Some(BTreeMap::new()));

    // Check a repository left without collaborators matches an unset list
    let actual_state = State {
        repositories: vec![Repository {
            name: "repo2".to_string(),
            visibility: Some(Visibility::Public),
            ..Default::default()
        }],
        ..Default::default()
    };
    let desired_state = State {
        repositories: vec![state.repositories[1].clone()],
        ..Default::default()
    };
    assert_eq!(actual_state.diff(&desired_state), Changes::default());
}

#[tokio::test]
async fn test_new_from_service_builds_repository() {
    // Setup service
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        admins: vec!["admin1"],
        repositories: vec![ServiceRepository {
            collaborators: vec![("admin1", Role::Admin), ("user1", Role::Write)],
            invitations: vec![
                sample_repository_invitation(1, Some("invited1"), &Role::Triage),
                sample_repository_invitation(2, None, &Role::Admin),
            ],
            teams: vec![("team-slug", Role::Maintain)],
            visibility: "private",
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_service_load(&mut svc, &service_state);

    // Build the state
    let state = State::new_from_service(Arc::new(svc), &sample_ctx()).await.unwrap();

    // Check admins and invitations without invitee were left out
    assert_eq!(
        state.repositories,
        vec![Repository {
            name: "repo1".to_string(),
            collaborators: Some(BTreeMap::from([
                ("invited1".to_string(), Role::Triage),
                ("user1".to_string(), Role::Write),
            ])),
            teams: Some(BTreeMap::from([("team-slug".to_string(), Role::Maintain)])),
            visibility: Some(Visibility::Private),
        }]
    );
}

#[tokio::test]
async fn test_new_from_service_builds_teams_from_slugs_and_pending_invitations() {
    // Setup service
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        teams: vec![ServiceTeam {
            invitations: vec![
                ("", None),
                (
                    "active1",
                    Some(sample_team_membership(
                        TeamMembershipRole::Member,
                        OrgMembershipState::Active,
                    )),
                ),
                (
                    "pending-maintainer",
                    Some(sample_team_membership(
                        TeamMembershipRole::Maintainer,
                        OrgMembershipState::Pending,
                    )),
                ),
                (
                    "pending-member",
                    Some(sample_team_membership(
                        TeamMembershipRole::Member,
                        OrgMembershipState::Pending,
                    )),
                ),
            ],
            members: vec!["member1"],
            name: "My Team",
            ..ServiceTeam::new("my-team", "maintainer1")
        }],
        ..Default::default()
    };
    expect_service_load(&mut svc, &service_state);

    // Build the state
    let state = State::new_from_service(Arc::new(svc), &sample_ctx()).await.unwrap();

    // Check pending users were placed by role and others left out
    assert_eq!(
        state.directory.teams,
        vec![Team {
            name: "my-team".to_string(),
            display_name: Some("My Team".to_string()),
            maintainers: vec!["maintainer1".to_string(), "pending-maintainer".to_string()],
            members: vec!["member1".to_string(), "pending-member".to_string()],
            ..Default::default()
        }]
    );
}

#[tokio::test]
async fn test_new_from_service_excludes_archived_repositories_and_temporary_forks() {
    // Setup service
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![
            ServiceRepository {
                archived: true,
                ..ServiceRepository::new("archived-repo")
            },
            ServiceRepository::new("repo1"),
            ServiceRepository {
                ignored: true,
                ..ServiceRepository::new("repo1-ghsa-2345-cfgh-jmpq")
            },
            ServiceRepository::new("repo1-ghsa-abcd-efgh-ijkl"),
        ],
        ..Default::default()
    };
    expect_service_load(&mut svc, &service_state);

    // Build the state
    let state = State::new_from_service(Arc::new(svc), &sample_ctx()).await.unwrap();

    // Check the repository whose name only resembles a fork was kept
    assert_eq!(
        repository_names(&state),
        vec!["repo1", "repo1-ghsa-abcd-efgh-ijkl"]
    );
}

#[tokio::test]
async fn test_new_from_service_fails_when_collaborators_cannot_be_listed() {
    // Setup service
    let mut svc = MockSvc::new();
    svc.expect_list_teams().times(1).returning(|_| Box::pin(future::ready(Ok(vec![]))));
    svc.expect_list_org_admins().times(1).returning(|_| Box::pin(future::ready(Ok(vec![]))));
    svc.expect_list_repositories().times(1).returning(|_| {
        Box::pin(future::ready(Ok(vec![sample_repository(
            "repo1", "public", false,
        )])))
    });
    svc.expect_list_repository_collaborators()
        .withf(|ctx, repo_name| ctx.org == ORG && repo_name == "repo1")
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!("boom")))));

    // Build the state
    let err = State::new_from_service(Arc::new(svc), &sample_ctx()).await.unwrap_err();

    // Check the failure identifies the repository
    assert_eq!(
        format!("{err:#}"),
        "error getting repository info: error listing repository repo1 collaborators: boom"
    );
}

#[tokio::test]
async fn test_new_from_service_keeps_admin_role_of_invited_collaborator() {
    // Setup service
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            invitations: vec![sample_repository_invitation(1, Some("invited1"), &Role::Admin)],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_service_load(&mut svc, &service_state);

    // Build the state
    let state = State::new_from_service(Arc::new(svc), &sample_ctx()).await.unwrap();

    // Check the invitation role was used for the collaborator
    assert_eq!(
        state.repositories[0].collaborators,
        Some(BTreeMap::from([("invited1".to_string(), Role::Admin)]))
    );
}

#[tokio::test]
async fn test_new_from_service_leaves_empty_collaborators_and_teams_unset() {
    // Setup service
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        admins: vec!["admin1"],
        repositories: vec![ServiceRepository {
            collaborators: vec![("admin1", Role::Admin)],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_service_load(&mut svc, &service_state);

    // Build the state
    let state = State::new_from_service(Arc::new(svc), &sample_ctx()).await.unwrap();

    // Check both collections were left unset
    assert_eq!(
        state.repositories,
        vec![Repository {
            name: "repo1".to_string(),
            visibility: Some(Visibility::Public),
            ..Default::default()
        }]
    );
}

#[test]
fn test_repository_change_keywords_include_action_and_names() {
    // Setup each change and the words it must be found by
    let repo = || "repo1".to_string();
    let team = || "team1".to_string();
    let user = || "user1".to_string();
    let cases = [
        (
            RepositoryChange::RepositoryAdded(Repository {
                name: repo(),
                collaborators: Some(BTreeMap::from([(user(), Role::Read)])),
                teams: Some(BTreeMap::from([(team(), Role::Write)])),
                ..Default::default()
            }),
            vec!["repository", "added", "repo1", "team1", "user1"],
        ),
        (
            RepositoryChange::TeamAdded(repo(), team(), Role::Write),
            vec!["repository", "team", "added", "repo1", "team1"],
        ),
        (
            RepositoryChange::TeamRemoved(repo(), team()),
            vec!["repository", "team", "removed", "repo1", "team1"],
        ),
        (
            RepositoryChange::TeamRoleUpdated(repo(), team(), Role::Write),
            vec!["repository", "team", "updated", "repo1", "team1"],
        ),
        (
            RepositoryChange::CollaboratorAdded(repo(), user(), Role::Write),
            vec!["repository", "collaborator", "added", "repo1", "user1"],
        ),
        (
            RepositoryChange::CollaboratorRemoved(repo(), user()),
            vec!["repository", "collaborator", "removed", "repo1", "user1"],
        ),
        (
            RepositoryChange::CollaboratorRoleUpdated(repo(), user(), Role::Write),
            vec!["repository", "collaborator", "updated", "repo1", "user1"],
        ),
        (
            RepositoryChange::VisibilityUpdated(repo(), Visibility::Private),
            vec!["repository", "visibility", "updated", "repo1"],
        ),
    ];

    // Check each change can be found by those words
    for (change, expected_keywords) in cases {
        let keywords = change.keywords();
        for keyword in expected_keywords {
            assert!(keywords.contains(&keyword), "{change:?} misses {keyword}");
        }
    }
}

#[test]
fn test_role_from_invitation_permissions_maps_all_variants() {
    for (permissions, role) in [
        (RepositoryInvitationPermissions::Read, Role::Read),
        (RepositoryInvitationPermissions::Triage, Role::Triage),
        (RepositoryInvitationPermissions::Write, Role::Write),
        (RepositoryInvitationPermissions::Maintain, Role::Maintain),
        (RepositoryInvitationPermissions::Admin, Role::Admin),
        (RepositoryInvitationPermissions::Noop, Role::Read),
        (RepositoryInvitationPermissions::FallthroughString, Role::Read),
    ] {
        assert_eq!(Role::from(permissions), role);
    }
}

#[test]
fn test_role_from_repository_permissions_defaults_to_read() {
    // Setup permissions with all the flags unset
    let permissions: RepositoryPermissions = serde_json::from_value(json!({})).unwrap();

    // Check the default role is used, also when there are no permissions
    assert_eq!(Role::from(Some(permissions)), Role::Read);
    assert_eq!(Role::from(None::<RepositoryPermissions>), Role::Read);
}

#[test]
fn test_role_from_repository_permissions_uses_highest_flag() {
    for role in all_roles() {
        let permissions: RepositoryPermissions = serde_json::from_value(sample_permissions(&role)).unwrap();
        assert_eq!(Role::from(Some(permissions)), role);
    }
}

#[test]
fn test_role_from_team_permissions_defaults_to_read() {
    // Setup permissions with all the flags unset
    let permissions: TeamPermissions = serde_json::from_value(json!({})).unwrap();

    // Check the default role is used, also when there are no permissions
    assert_eq!(Role::from(Some(permissions)), Role::Read);
    assert_eq!(Role::from(None::<TeamPermissions>), Role::Read);
}

#[test]
fn test_role_from_team_permissions_uses_highest_flag() {
    for role in all_roles() {
        let permissions: TeamPermissions = serde_json::from_value(sample_permissions(&role)).unwrap();
        assert_eq!(Role::from(Some(permissions)), role);
    }
}

#[test]
fn test_role_into_invitation_permissions_uses_invitation_vocabulary() {
    // Setup the wire value expected for each role
    let expected = ["read", "triage", "write", "maintain", "admin"];

    // Check the value sent to GitHub for each role
    for (role, expected) in all_roles().iter().zip(expected) {
        let permissions = RepositoryInvitationPermissions::from(role);
        assert_eq!(serde_json::to_value(permissions).unwrap(), expected);
    }
}

#[test]
fn test_role_into_team_permission_uses_team_vocabulary() {
    // Setup the wire value expected for each role
    let expected = ["pull", "triage", "push", "maintain", "admin"];

    // Check the value sent to GitHub for each role
    for (role, expected) in all_roles().iter().zip(expected) {
        let permission = TeamsAddUpdateRepoPermissionsInOrgRequestPermission::from(role);
        assert_eq!(serde_json::to_value(permission).unwrap(), expected);
    }
}

#[tokio::test]
async fn test_validate_accepts_collaborator_with_equal_or_higher_role() {
    // Setup a repository where collaborators have at least their team's role
    let state = State {
        directory: sample_directory(),
        repositories: vec![Repository {
            name: "repo1".to_string(),
            collaborators: Some(BTreeMap::from([
                ("maintainer1".to_string(), Role::Write),
                ("member1".to_string(), Role::Admin),
            ])),
            teams: Some(BTreeMap::from([("team1".to_string(), Role::Write)])),
            ..Default::default()
        }],
    };

    // Validate the state
    let result = validate(&state, vec!["maintainer1"]).await;

    // Check the state is valid
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_validate_rejects_collaborator_below_highest_team_role() {
    // Setup the role for each team, trying the highest one in both positions
    let cases = [
        (Role::Read, Role::Admin, "team2"),
        (Role::Admin, Role::Read, "team1"),
    ];

    for (team1_role, team2_role, highest_team) in cases {
        // Setup a user that belongs to both teams
        let mut directory = sample_directory();
        directory.teams.push(Team {
            name: "team2".to_string(),
            maintainers: vec!["maintainer1".to_string()],
            members: vec!["member1".to_string()],
            ..Default::default()
        });
        let state = State {
            directory,
            repositories: vec![Repository {
                name: "repo1".to_string(),
                collaborators: Some(BTreeMap::from([("member1".to_string(), Role::Write)])),
                teams: Some(BTreeMap::from([
                    ("team1".to_string(), team1_role),
                    ("team2".to_string(), team2_role),
                ])),
                ..Default::default()
            }],
        };

        // Validate the state
        let err = validate(&state, vec!["maintainer1"]).await.unwrap_err();

        // Check the error names the team providing the highest role
        assert_eq!(
            multierror_messages(&err),
            vec![format!(
                "repo[repo1]: collaborator member1 already has admin access from team {highest_team}"
            )]
        );
    }
}

#[tokio::test]
async fn test_validate_rejects_collaborator_below_team_role() {
    // Setup the user to check, as a member and as a maintainer of the team
    for user_name in ["maintainer1", "member1"] {
        // Setup a collaborator with less privileges than the team provides
        let state = State {
            directory: sample_directory(),
            repositories: vec![Repository {
                name: "repo1".to_string(),
                collaborators: Some(BTreeMap::from([(user_name.to_string(), Role::Read)])),
                teams: Some(BTreeMap::from([("team1".to_string(), Role::Write)])),
                ..Default::default()
            }],
        };

        // Validate the state
        let err = validate(&state, vec!["maintainer1"]).await.unwrap_err();

        // Check the error names the collaborator and the team
        assert_eq!(
            multierror_messages(&err),
            vec![format!(
                "repo[repo1]: collaborator {user_name} already has write access from team team1"
            )]
        );
    }
}

#[tokio::test]
async fn test_validate_rejects_maintainer_outside_organization() {
    // Setup a state whose team maintainer is not an organization member
    let state = State {
        directory: sample_directory(),
        ..Default::default()
    };

    // Validate the state
    let err = validate(&state, vec!["user1"]).await.unwrap_err();

    // Check the maintainer was reported
    assert_eq!(
        multierror_messages(&err),
        vec!["team[team1]: maintainer1 must be an organization member to be a maintainer"]
    );
}

#[tokio::test]
async fn test_validate_rejects_repository_with_unknown_team() {
    // Setup a repository using a team that is not in the directory
    let state = State {
        directory: sample_directory(),
        repositories: vec![Repository {
            name: "repo1".to_string(),
            teams: Some(BTreeMap::from([("unknown".to_string(), Role::Read)])),
            ..Default::default()
        }],
    };

    // Validate the state
    let err = validate(&state, vec!["maintainer1"]).await.unwrap_err();

    // Check the team was reported
    assert_eq!(
        multierror_messages(&err),
        vec!["repo[repo1]: team unknown does not exist in directory"]
    );
}

#[tokio::test]
async fn test_validate_reports_all_errors() {
    // Setup a state with several problems
    let state = State {
        directory: sample_directory(),
        repositories: vec![Repository {
            name: "repo1".to_string(),
            teams: Some(BTreeMap::from([("unknown".to_string(), Role::Read)])),
            ..Default::default()
        }],
    };

    // Validate the state
    let err = validate(&state, vec![]).await.unwrap_err();

    // Check all problems were reported
    assert_eq!(
        multierror_messages(&err),
        vec![
            "repo[repo1]: team unknown does not exist in directory",
            "team[team1]: maintainer1 must be an organization member to be a maintainer",
        ]
    );
}

#[test]
fn test_visibility_from_string_maps_github_values() {
    // Setup the value expected for each string, unknown ones being public
    let cases = [
        ("", Visibility::Public),
        ("internal", Visibility::Internal),
        ("private", Visibility::Private),
        ("public", Visibility::Public),
        ("unknown", Visibility::Public),
    ];

    // Check the visibility obtained for each of them
    for (value, expected) in cases {
        assert_eq!(Visibility::from(value.to_string()), expected);
    }
}

// Helpers.

/// Return all the roles, from the lowest to the highest one.
fn all_roles() -> [Role; 5] {
    [Role::Read, Role::Triage, Role::Write, Role::Maintain, Role::Admin]
}

/// Build a state from the configuration using the mocks provided.
async fn new_from_config(gh: MockGH, svc: MockSvc) -> Result<State> {
    State::new_from_config(
        Arc::new(gh),
        Arc::new(svc),
        &sample_org().legacy,
        &sample_ctx(),
        &sample_src(),
    )
    .await
}

/// Return the names of the repositories in the state provided.
fn repository_names(state: &State) -> Vec<&str> {
    state.repositories.iter().map(|repo| repo.name.as_str()).collect()
}

/// Build a directory with a team that has a maintainer and a member.
fn sample_directory() -> Directory {
    Directory {
        teams: vec![Team {
            name: "team1".to_string(),
            maintainers: vec!["maintainer1".to_string()],
            members: vec!["member1".to_string()],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Validate the state provided against the organization members given.
async fn validate(state: &State, org_members: Vec<&str>) -> Result<()> {
    let org_members: Vec<_> = org_members.into_iter().map(sample_user).collect();
    let mut svc = MockSvc::new();
    svc.expect_list_org_members()
        .withf(|ctx| ctx.org == ORG)
        .times(1)
        .returning(move |_| Box::pin(future::ready(Ok(org_members.clone()))));
    state.validate(Arc::new(svc), &sample_ctx()).await
}
