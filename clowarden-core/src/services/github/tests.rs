use std::{collections::BTreeMap, future, sync::Arc};

use anyhow::format_err;
use mockall::Sequence;
use octorust::types::{OrgMembershipState, TeamMembershipRole};

use crate::{
    directory::{DirectoryChange, Team},
    github::MockGH,
    multierror::MultiError,
    services::{BaseRefConfigStatus, Change, ChangeApplied, ChangesSummary, ServiceHandler},
};

use super::{
    Handler,
    service::MockSvc,
    state::{Changes, Repository, RepositoryChange, Role, Visibility},
    testing::{
        BASE_REF, HEAD_REF, ORG, ServiceRepository, ServiceState, ServiceTeam, expect_config_file,
        expect_config_file_error, expect_config_load, expect_failed_config_load, expect_no_mutations,
        expect_service_load, multierror_messages, sample_ctx, sample_head_src, sample_org, sample_repository,
        sample_repository_invitation, sample_team_membership,
    },
};

/// Configuration used as the base one in the changes summary tests.
const BASE_CONFIG: &str = "
teams:
  - name: team1
    maintainers:
      - maintainer1
repositories:
  - name: repo1
    external_collaborators:
      removed1: read
";

/// Error returned by the mocks.
const ERROR: &str = "boom";

/// Configuration used as the head one in the changes summary tests. Compared
/// to the base one it adds a team member, and replaces a collaborator.
const HEAD_CONFIG: &str = "
teams:
  - name: team1
    maintainers:
      - maintainer1
    members:
      - member1
repositories:
  - name: repo1
    external_collaborators:
      user1: write
";

/// Configuration with a repository where only `other1` is a collaborator.
const INVITATIONS_CONFIG: &str = "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      other1: read
";

/// Configuration with a repository where `user1` has the admin role.
const INVITATIONS_ROLE_UPDATED_CONFIG: &str = "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      other1: read
      user1: admin
";

/// Configuration matching a service state in which each of the normalizations
/// applied when loading the states comes into play.
const NORMALIZED_CONFIG: &str = "
teams:
  - name: my-team
    maintainers:
      - maintainer1
    members:
      - admin1
      - member1
      - pending1
repositories:
  - name: archived-repo
    teams:
      my-team: write
  - name: repo1
    external_collaborators:
      admin1: admin
      invited1: read
      user1: write
    teams:
      my-team: write
  - name: repo2
    teams: {}
    visibility: private
";

/// Configuration without teams, that keeps the repository.
const TEAM_DROPPED_CONFIG: &str = "
teams: []
repositories:
  - name: repo1
";

/// Configuration with a team member and a collaborator that do not exist.
const UNKNOWN_USERS_CONFIG: &str = "
teams:
  - name: team1
    maintainers:
      - maintainer1
    members:
      - unknown1
repositories:
  - name: repo1
    external_collaborators:
      unknown2: write
";

/// Id of the pending invitation of `user1`.
const USER_INVITATION_ID: i64 = 7;

#[tokio::test]
async fn test_get_changes_summary_fails_when_head_config_is_invalid() {
    // Setup configuration, the base one must not be read
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, HEAD_REF, 1, "not: [valid");
    gh.expect_get_file_content().withf(|src, _| src.ref_ == BASE_REF).never();

    // Setup service
    let mut svc = MockSvc::new();
    expect_failed_config_load(&mut svc, &sample_service_state());
    svc.expect_get_user_login().never();

    // Get the changes summary
    let result = handler(gh, svc).get_changes_summary(&sample_org(), &sample_head_src()).await;

    // Check the configuration was reported as invalid
    assert_eq!(
        result.err().unwrap().to_string(),
        "invalid directory configuration"
    );
}

#[tokio::test]
async fn test_get_changes_summary_fails_when_head_user_is_invalid() {
    // Setup configuration
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, HEAD_REF, 2, HEAD_CONFIG);
    expect_config_file(&mut gh, BASE_REF, 2, BASE_CONFIG);

    // Setup service
    let mut svc = MockSvc::new();
    let service_state = sample_service_state();
    expect_config_load(&mut svc, &service_state);
    expect_config_load(&mut svc, &service_state);
    expect_user_login(&mut svc, "member1", "member1");
    expect_user_login(&mut svc, "user1", "User1");

    // Get the changes summary
    let result = handler(gh, svc).get_changes_summary(&sample_org(), &sample_head_src()).await;

    // Check the invalid user was reported
    assert_eq!(
        multierror_messages(&result.err().unwrap()),
        vec!["user[user1]: invalid username, should be User1"]
    );
}

#[tokio::test]
async fn test_get_changes_summary_returns_repository_changes_from_base_to_head() {
    // Setup configuration
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, HEAD_REF, 2, HEAD_CONFIG);
    expect_config_file(&mut gh, BASE_REF, 2, BASE_CONFIG);

    // Setup service, the team member must be validated too
    let mut svc = MockSvc::new();
    let service_state = sample_service_state();
    expect_config_load(&mut svc, &service_state);
    expect_config_load(&mut svc, &service_state);
    expect_user_login(&mut svc, "member1", "member1");
    expect_user_login(&mut svc, "user1", "user1");
    svc.expect_get_user_login().withf(|_, user_name| user_name == "removed1").never();

    // Get the changes summary
    let summary = handler(gh, svc).get_changes_summary(&sample_org(), &sample_head_src()).await.unwrap();

    // Check only repositories changes are returned, in the right direction
    assert_eq!(summary.base_ref_config_status, BaseRefConfigStatus::Valid);
    assert_summary_changes(
        &summary,
        &[
            RepositoryChange::CollaboratorRemoved("repo1".to_string(), "removed1".to_string()),
            RepositoryChange::CollaboratorAdded("repo1".to_string(), "user1".to_string(), Role::Write),
        ],
    );
}

#[tokio::test]
async fn test_get_changes_summary_tolerates_unreadable_base_reporting_user_problems_as_warnings() {
    // Setup configuration, the base one cannot be fetched
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, HEAD_REF, 2, UNKNOWN_USERS_CONFIG);
    expect_config_file_error(&mut gh, BASE_REF, ERROR);

    // Setup service, all the users in the head are looked up
    let mut svc = MockSvc::new();
    let service_state = sample_service_state();
    expect_config_load(&mut svc, &service_state);
    expect_failed_config_load(&mut svc, &service_state);
    expect_user_login(&mut svc, "unknown1", "Unknown1");
    svc.expect_get_user_login()
        .times(1)
        .withf(|_, user_name| user_name == "unknown2")
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Get the changes summary
    let summary = handler(gh, svc).get_changes_summary(&sample_org(), &sample_head_src()).await.unwrap();

    // Check the problems were reported without failing the validation
    assert_eq!(summary.base_ref_config_status, BaseRefConfigStatus::Invalid);
    assert!(summary.changes.is_empty());
    assert_eq!(
        summary.warnings,
        vec![
            "user[unknown1]: invalid username, should be Unknown1".to_string(),
            format!("user[unknown2]: error validating username: {ERROR}"),
        ]
    );
}

#[tokio::test]
async fn test_reconcile_adds_team_before_adding_it_to_repository() {
    // Setup configuration
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
repositories:
  - name: repo1
    teams:
      team1: write
",
    );

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        members: vec!["maintainer1"],
        repositories: vec![ServiceRepository::new("repo1")],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected, in order
    let mut seq = Sequence::new();
    svc.expect_add_team()
        .withf(|ctx, team| ctx.org == ORG && team.name == "team1" && team.maintainers == vec!["maintainer1"])
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_team()
        .withf(|ctx, repo_name, team_name, role| {
            ctx.org == ORG && repo_name == "repo1" && team_name == "team1" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check the changes applied
    assert_eq!(changes_applied.len(), 2);
    assert_change_applied(
        &changes_applied[0],
        &DirectoryChange::TeamAdded(Team {
            name: "team1".to_string(),
            maintainers: vec!["maintainer1".to_string()],
            ..Default::default()
        }),
        None,
    );
    assert_change_applied(
        &changes_applied[1],
        &RepositoryChange::TeamAdded("repo1".to_string(), "team1".to_string(), Role::Write),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_applies_nothing_when_service_matches_config() {
    // Setup configuration
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 2, NORMALIZED_CONFIG);

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        admins: vec!["admin1"],
        members: vec!["admin1", "maintainer1", "member1"],
        repositories: vec![
            ServiceRepository {
                archived: true,
                ..ServiceRepository::new("archived-repo")
            },
            ServiceRepository {
                collaborators: vec![("admin1", Role::Admin), ("user1", Role::Write)],
                invitations: vec![sample_repository_invitation(1, Some("invited1"), &Role::Read)],
                teams: vec![("my-team", Role::Write)],
                ..ServiceRepository::new("repo1")
            },
            ServiceRepository {
                ignored: true,
                ..ServiceRepository::new("repo1-ghsa-2345-cfgh-jmpq")
            },
            ServiceRepository {
                visibility: "private",
                ..ServiceRepository::new("repo2")
            },
        ],
        teams: vec![ServiceTeam {
            invitations: vec![(
                "pending1",
                Some(sample_team_membership(
                    TeamMembershipRole::Member,
                    OrgMembershipState::Pending,
                )),
            )],
            maintainers: vec!["admin1", "maintainer1"],
            members: vec!["member1"],
            name: "My Team",
            ..ServiceTeam::new("my-team", "maintainer1")
        }],
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected
    expect_no_mutations(&mut svc);

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check no changes were applied
    assert!(changes_applied.is_empty());
}

#[tokio::test]
async fn test_reconcile_continues_after_failed_change() {
    // Setup configuration
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
repositories:
  - name: repo1
    external_collaborators:
      user1: write
    teams:
      team1: write
",
    );

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        members: vec!["maintainer1"],
        repositories: vec![ServiceRepository {
            visibility: "private",
            ..ServiceRepository::new("repo1")
        }],
        teams: vec![ServiceTeam::new("team1", "maintainer1")],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected, the one in the middle fails
    svc.expect_add_repository_team()
        .withf(|ctx, repo_name, team_name, role| {
            ctx.org == ORG && repo_name == "repo1" && team_name == "team1" && *role == Role::Write
        })
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user1" && *role == Role::Write
        })
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    svc.expect_update_repository_visibility()
        .withf(|ctx, repo_name, visibility| {
            ctx.org == ORG && repo_name == "repo1" && *visibility == Visibility::Public
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check all changes were recorded, and only one has an error
    assert_eq!(changes_applied.len(), 3);
    assert_change_applied(
        &changes_applied[0],
        &RepositoryChange::TeamAdded("repo1".to_string(), "team1".to_string(), Role::Write),
        None,
    );
    assert_change_applied(
        &changes_applied[1],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user1".to_string(), Role::Write),
        Some(ERROR),
    );
    assert_change_applied(
        &changes_applied[2],
        &RepositoryChange::VisibilityUpdated("repo1".to_string(), Visibility::Public),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_fails_without_mutations_when_config_cannot_be_read() {
    // Setup configuration
    let mut gh = MockGH::new();
    expect_config_file_error(&mut gh, BASE_REF, ERROR);

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = sample_service_state();
    expect_service_load(&mut svc, &service_state);
    expect_failed_config_load(&mut svc, &service_state);

    // Setup mutations expected
    expect_no_mutations(&mut svc);

    // Run the reconciliation
    let err = handler(gh, svc).reconcile(&sample_org()).await.unwrap_err();

    // Check the desired state was reported as unavailable
    assert_eq!(err.to_string(), "error getting desired state from configuration");
}

#[tokio::test]
async fn test_reconcile_fails_without_mutations_when_config_is_invalid() {
    // Setup a configuration using a team that does not exist
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        2,
        "
teams: []
repositories:
  - name: repo1
    teams:
      unknown: write
",
    );

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository::new("repo1")],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected
    expect_no_mutations(&mut svc);

    // Run the reconciliation
    let err = handler(gh, svc).reconcile(&sample_org()).await.unwrap_err();

    // Check the configuration problem was reported
    assert_eq!(err.to_string(), "error getting desired state from configuration");
    assert_eq!(
        multierror_messages(&err),
        vec!["repo[repo1]: team unknown does not exist in directory"]
    );
}

#[tokio::test]
async fn test_reconcile_fails_without_mutations_when_service_state_cannot_be_read() {
    // Setup configuration, which must not be needed
    let mut gh = MockGH::new();
    gh.expect_get_file_content().never();

    // Setup service state, a repository's teams cannot be listed
    let mut svc = MockSvc::new();
    svc.expect_list_teams().times(1).returning(|_| Box::pin(future::ready(Ok(vec![]))));
    svc.expect_list_org_admins().times(1).returning(|_| Box::pin(future::ready(Ok(vec![]))));
    svc.expect_list_repositories().times(1).returning(|_| {
        Box::pin(future::ready(Ok(vec![sample_repository(
            "repo1", "public", false,
        )])))
    });
    svc.expect_list_repository_collaborators()
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(vec![]))));
    svc.expect_list_repository_invitations()
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(vec![]))));
    svc.expect_list_repository_teams()
        .withf(|ctx, repo_name| ctx.org == ORG && repo_name == "repo1")
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Setup mutations expected
    expect_no_mutations(&mut svc);

    // Run the reconciliation
    let err = handler(gh, svc).reconcile(&sample_org()).await.unwrap_err();

    // Check the actual state was reported as unavailable
    assert_eq!(err.to_string(), "error getting actual state from service");
}

#[tokio::test]
async fn test_reconcile_records_invitation_lookup_failure_on_collaborator_removal() {
    // Setup a configuration that drops a team and a collaborator
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

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            collaborators: vec![("user1", Role::Write)],
            visibility: "private",
            ..ServiceRepository::new("repo1")
        }],
        teams: vec![ServiceTeam::new("team1", "maintainer1")],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup the invitation lookup, which comes after the state load one
    expect_invitations_lookup_error(&mut svc);

    // Setup mutations expected
    svc.expect_remove_team()
        .withf(|ctx, team_name| ctx.org == ORG && team_name == "team1")
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));
    svc.expect_remove_repository_collaborator().never();
    svc.expect_remove_repository_invitation().never();
    svc.expect_update_repository_visibility()
        .withf(|ctx, repo_name, visibility| {
            ctx.org == ORG && repo_name == "repo1" && *visibility == Visibility::Public
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check applied changes survive and the following one was attempted
    assert_eq!(changes_applied.len(), 3);
    assert_change_applied(
        &changes_applied[0],
        &DirectoryChange::TeamRemoved("team1".to_string()),
        None,
    );
    assert_change_applied(
        &changes_applied[1],
        &RepositoryChange::CollaboratorRemoved("repo1".to_string(), "user1".to_string()),
        Some(ERROR),
    );
    assert_change_applied(
        &changes_applied[2],
        &RepositoryChange::VisibilityUpdated("repo1".to_string(), Visibility::Public),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_records_invitation_lookup_failure_on_collaborator_role_update() {
    // Setup a configuration that adds a member and updates a collaborator
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
      - member1
repositories:
  - name: repo1
    external_collaborators:
      user1: read
",
    );

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        members: vec!["maintainer1"],
        repositories: vec![ServiceRepository {
            collaborators: vec![("user1", Role::Write)],
            visibility: "private",
            ..ServiceRepository::new("repo1")
        }],
        teams: vec![ServiceTeam::new("team1", "maintainer1")],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup the invitation lookup, which comes after the state load one
    expect_invitations_lookup_error(&mut svc);

    // Setup mutations expected
    svc.expect_add_team_member()
        .withf(|ctx, team_name, user_name| ctx.org == ORG && team_name == "team1" && user_name == "member1")
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_update_repository_collaborator_role().never();
    svc.expect_update_repository_invitation().never();
    svc.expect_update_repository_visibility()
        .withf(|ctx, repo_name, visibility| {
            ctx.org == ORG && repo_name == "repo1" && *visibility == Visibility::Public
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check applied changes survive and the following one was attempted
    assert_eq!(changes_applied.len(), 3);
    assert_change_applied(
        &changes_applied[0],
        &DirectoryChange::TeamMemberAdded("team1".to_string(), "member1".to_string()),
        None,
    );
    assert_change_applied(
        &changes_applied[1],
        &RepositoryChange::CollaboratorRoleUpdated("repo1".to_string(), "user1".to_string(), Role::Read),
        Some(ERROR),
    );
    assert_change_applied(
        &changes_applied[2],
        &RepositoryChange::VisibilityUpdated("repo1".to_string(), Visibility::Public),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_removes_collaborator_without_pending_invitation() {
    // Setup a configuration that drops a collaborator
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 2, INVITATIONS_CONFIG);

    // Setup service state, with invitations that do not belong to the user
    let mut svc = MockSvc::new();
    let service_state = sample_invitations_service_state(("user1", Role::Write), false);
    expect_reconcile_load(&mut svc, &service_state);
    expect_invitations_lookup(&mut svc, &service_state);

    // Setup mutations expected
    svc.expect_remove_repository_collaborator()
        .withf(|ctx, repo_name, user_name| ctx.org == ORG && repo_name == "repo1" && user_name == "user1")
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_remove_repository_invitation().never();

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check the change applied
    assert_eq!(changes_applied.len(), 1);
    assert_change_applied(
        &changes_applied[0],
        &RepositoryChange::CollaboratorRemoved("repo1".to_string(), "user1".to_string()),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_removes_maintainer_before_adding_member() {
    // Setup a configuration where a maintainer is demoted to member
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
      - user1
repositories: []
",
    );

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        members: vec!["maintainer1"],
        teams: vec![ServiceTeam {
            maintainers: vec!["maintainer1", "user1"],
            ..ServiceTeam::new("team1", "maintainer1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected, in order
    let mut seq = Sequence::new();
    svc.expect_remove_team_maintainer()
        .withf(|ctx, team_name, user_name| ctx.org == ORG && team_name == "team1" && user_name == "user1")
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_team_member()
        .withf(|ctx, team_name, user_name| ctx.org == ORG && team_name == "team1" && user_name == "user1")
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check the changes applied
    assert_eq!(changes_applied.len(), 2);
    assert_change_applied(
        &changes_applied[0],
        &DirectoryChange::TeamMaintainerRemoved("team1".to_string(), "user1".to_string()),
        None,
    );
    assert_change_applied(
        &changes_applied[1],
        &DirectoryChange::TeamMemberAdded("team1".to_string(), "user1".to_string()),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_removes_member_before_adding_maintainer() {
    // Setup a configuration where a member is promoted to maintainer
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
      - user1
repositories: []
",
    );

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        members: vec!["maintainer1", "user1"],
        teams: vec![ServiceTeam {
            members: vec!["user1"],
            ..ServiceTeam::new("team1", "maintainer1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected, in order
    let mut seq = Sequence::new();
    svc.expect_remove_team_member()
        .withf(|ctx, team_name, user_name| ctx.org == ORG && team_name == "team1" && user_name == "user1")
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_team_maintainer()
        .withf(|ctx, team_name, user_name| ctx.org == ORG && team_name == "team1" && user_name == "user1")
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check the changes applied
    assert_eq!(changes_applied.len(), 2);
    assert_change_applied(
        &changes_applied[0],
        &DirectoryChange::TeamMemberRemoved("team1".to_string(), "user1".to_string()),
        None,
    );
    assert_change_applied(
        &changes_applied[1],
        &DirectoryChange::TeamMaintainerAdded("team1".to_string(), "user1".to_string()),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_removes_pending_invitation_of_removed_collaborator() {
    // Setup a configuration that drops a collaborator
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 2, INVITATIONS_CONFIG);

    // Setup service state, where the user has a pending invitation
    let mut svc = MockSvc::new();
    let service_state = sample_invitations_service_state(("other1", Role::Read), true);
    expect_reconcile_load(&mut svc, &service_state);
    expect_invitations_lookup(&mut svc, &service_state);

    // Setup mutations expected
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| {
            ctx.org == ORG && repo_name == "repo1" && *invitation_id == USER_INVITATION_ID
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_remove_repository_collaborator().never();

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check the change applied
    assert_eq!(changes_applied.len(), 1);
    assert_change_applied(
        &changes_applied[0],
        &RepositoryChange::CollaboratorRemoved("repo1".to_string(), "user1".to_string()),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_removes_team_from_repository_when_other_team_is_removed() {
    // Setup a configuration where one team is dropped and the other one stays
    // but leaves the repository
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
repositories:
  - name: repo1
",
    );

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        members: vec!["maintainer1"],
        repositories: vec![ServiceRepository {
            teams: vec![("team1", Role::Write), ("team2", Role::Write)],
            ..ServiceRepository::new("repo1")
        }],
        teams: vec![
            ServiceTeam::new("team1", "maintainer1"),
            ServiceTeam::new("team2", "maintainer1"),
        ],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected
    svc.expect_remove_team()
        .withf(|ctx, team_name| ctx.org == ORG && team_name == "team2")
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));
    svc.expect_remove_repository_team()
        .withf(|ctx, repo_name, team_name| ctx.org == ORG && repo_name == "repo1" && team_name == "team1")
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check only the kept team was removed from the repository
    assert_eq!(changes_applied.len(), 2);
    assert_change_applied(
        &changes_applied[0],
        &DirectoryChange::TeamRemoved("team2".to_string()),
        None,
    );
    assert_change_applied(
        &changes_applied[1],
        &RepositoryChange::TeamRemoved("repo1".to_string(), "team1".to_string()),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_removes_team_from_repository_when_team_is_kept() {
    // Setup a configuration where the team stays but leaves the repository
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
repositories:
  - name: repo1
",
    );

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = sample_team_in_repository_service_state();
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected
    svc.expect_remove_team().never();
    svc.expect_remove_repository_team()
        .withf(|ctx, repo_name, team_name| ctx.org == ORG && repo_name == "repo1" && team_name == "team1")
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check the change applied
    assert_eq!(changes_applied.len(), 1);
    assert_change_applied(
        &changes_applied[0],
        &RepositoryChange::TeamRemoved("repo1".to_string(), "team1".to_string()),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_removes_team_from_repository_when_team_removal_fails() {
    // Setup a configuration that drops the team
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 2, TEAM_DROPPED_CONFIG);

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = sample_team_in_repository_service_state();
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected, the team cannot be removed
    svc.expect_remove_team()
        .withf(|ctx, team_name| ctx.org == ORG && team_name == "team1")
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    svc.expect_remove_repository_team()
        .withf(|ctx, repo_name, team_name| ctx.org == ORG && repo_name == "repo1" && team_name == "team1")
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check both changes were recorded
    assert_eq!(changes_applied.len(), 2);
    assert_change_applied(
        &changes_applied[0],
        &DirectoryChange::TeamRemoved("team1".to_string()),
        Some(ERROR),
    );
    assert_change_applied(
        &changes_applied[1],
        &RepositoryChange::TeamRemoved("repo1".to_string(), "team1".to_string()),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_skips_repository_team_removal_for_removed_team() {
    // Setup a configuration that drops the team
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 2, TEAM_DROPPED_CONFIG);

    // Setup service state
    let mut svc = MockSvc::new();
    let service_state = sample_team_in_repository_service_state();
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected
    svc.expect_remove_team()
        .withf(|ctx, team_name| ctx.org == ORG && team_name == "team1")
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));
    svc.expect_remove_repository_team().never();

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check only the team removal was recorded
    assert_eq!(changes_applied.len(), 1);
    assert_change_applied(
        &changes_applied[0],
        &DirectoryChange::TeamRemoved("team1".to_string()),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_updates_collaborator_role_without_pending_invitation() {
    // Setup a configuration that updates a collaborator's role
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 2, INVITATIONS_ROLE_UPDATED_CONFIG);

    // Setup service state, with invitations that do not belong to the user
    let mut svc = MockSvc::new();
    let service_state = sample_invitations_service_state(("user1", Role::Write), false);
    expect_reconcile_load(&mut svc, &service_state);
    expect_invitations_lookup(&mut svc, &service_state);

    // Setup mutations expected
    svc.expect_update_repository_collaborator_role()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user1" && *role == Role::Admin
        })
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_update_repository_invitation().never();

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check the change applied
    assert_eq!(changes_applied.len(), 1);
    assert_change_applied(
        &changes_applied[0],
        &RepositoryChange::CollaboratorRoleUpdated("repo1".to_string(), "user1".to_string(), Role::Admin),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_updates_pending_invitation_of_updated_collaborator() {
    // Setup a configuration that updates a collaborator's role
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 2, INVITATIONS_ROLE_UPDATED_CONFIG);

    // Setup service state, where the user has a pending invitation
    let mut svc = MockSvc::new();
    let service_state = sample_invitations_service_state(("other1", Role::Read), true);
    expect_reconcile_load(&mut svc, &service_state);
    expect_invitations_lookup(&mut svc, &service_state);

    // Setup mutations expected
    svc.expect_update_repository_invitation()
        .withf(|ctx, repo_name, invitation_id, role| {
            ctx.org == ORG
                && repo_name == "repo1"
                && *invitation_id == USER_INVITATION_ID
                && *role == Role::Admin
        })
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_update_repository_collaborator_role().never();

    // Run the reconciliation
    let changes_applied = handler(gh, svc).reconcile(&sample_org()).await.unwrap();

    // Check the change applied
    assert_eq!(changes_applied.len(), 1);
    assert_change_applied(
        &changes_applied[0],
        &RepositoryChange::CollaboratorRoleUpdated("repo1".to_string(), "user1".to_string(), Role::Admin),
        None,
    );
}

#[tokio::test]
async fn test_validate_users_accepts_valid_usernames() {
    // Setup changes
    let changes = Changes {
        directory: vec![DirectoryChange::TeamMemberAdded(
            "team1".to_string(),
            "member1".to_string(),
        )],
        repositories: vec![RepositoryChange::CollaboratorAdded(
            "repo1".to_string(),
            "user1".to_string(),
            Role::Write,
        )],
    };

    // Setup service
    let mut svc = MockSvc::new();
    expect_user_login(&mut svc, "member1", "member1");
    expect_user_login(&mut svc, "user1", "user1");

    // Validate users
    let result = handler(MockGH::new(), svc).validate_users(&sample_ctx(), &changes).await;

    // Check validation succeeded
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_validate_users_checks_collaborators_of_new_repositories() {
    // Setup changes
    let changes = Changes {
        repositories: vec![RepositoryChange::RepositoryAdded(Repository {
            name: "repo1".to_string(),
            collaborators: Some(BTreeMap::from([("user1".to_string(), Role::Write)])),
            ..Default::default()
        })],
        ..Default::default()
    };

    // Setup service
    let mut svc = MockSvc::new();
    expect_user_login(&mut svc, "user1", "User1");

    // Validate users
    let err = handler(MockGH::new(), svc).validate_users(&sample_ctx(), &changes).await.unwrap_err();

    // Check the user was reported
    assert_eq!(
        multierror_messages(&err),
        vec!["user[user1]: invalid username, should be User1"]
    );
}

#[tokio::test]
async fn test_validate_users_checks_members_of_new_teams() {
    // Setup changes
    let changes = Changes {
        directory: vec![DirectoryChange::TeamAdded(Team {
            name: "team1".to_string(),
            maintainers: vec!["maintainer1".to_string()],
            members: vec!["member1".to_string()],
            ..Default::default()
        })],
        ..Default::default()
    };

    // Setup service, maintainers are checked when validating the state
    let mut svc = MockSvc::new();
    expect_user_login(&mut svc, "member1", "Member1");
    svc.expect_get_user_login().withf(|_, user_name| user_name == "maintainer1").never();

    // Validate users
    let err = handler(MockGH::new(), svc).validate_users(&sample_ctx(), &changes).await.unwrap_err();

    // Check the user was reported
    assert_eq!(
        multierror_messages(&err),
        vec!["user[member1]: invalid username, should be Member1"]
    );
}

#[tokio::test]
async fn test_validate_users_reports_all_invalid_usernames() {
    // Setup changes
    let changes = Changes {
        directory: vec![DirectoryChange::TeamMemberAdded(
            "team1".to_string(),
            "member1".to_string(),
        )],
        repositories: vec![RepositoryChange::CollaboratorAdded(
            "repo1".to_string(),
            "user1".to_string(),
            Role::Write,
        )],
    };

    // Setup service, one user has a different case and the other is unknown
    let mut svc = MockSvc::new();
    expect_user_login(&mut svc, "member1", "Member1");
    svc.expect_get_user_login()
        .withf(|ctx, user_name| ctx.org == ORG && user_name == "user1")
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Validate users
    let err = handler(MockGH::new(), svc).validate_users(&sample_ctx(), &changes).await.unwrap_err();

    // Check all problems were aggregated under the service context
    assert_eq!(
        err.downcast_ref::<MultiError>().unwrap().context.as_deref(),
        Some("invalid github service configuration")
    );
    assert_eq!(
        multierror_messages(&err),
        vec![
            "user[member1]: invalid username, should be Member1",
            "user[user1]: error validating username: boom",
        ]
    );
}

#[tokio::test]
async fn test_validate_users_skips_changes_without_users_to_validate() {
    // Setup changes
    let changes = Changes {
        directory: vec![DirectoryChange::TeamMaintainerAdded(
            "team1".to_string(),
            "maintainer1".to_string(),
        )],
        repositories: vec![RepositoryChange::RepositoryAdded(Repository {
            name: "repo1".to_string(),
            ..Default::default()
        })],
    };

    // Setup service
    let mut svc = MockSvc::new();
    svc.expect_get_user_login().never();

    // Validate users
    let result = handler(MockGH::new(), svc).validate_users(&sample_ctx(), &changes).await;

    // Check validation succeeded
    assert!(result.is_ok());
}

// Helpers.

/// Check the entry provided records the change and error given.
fn assert_change_applied(entry: &ChangeApplied, change: &dyn Change, error: Option<&str>) {
    assert_eq!(entry.change.details(), change.details());
    assert_eq!(entry.error.as_deref(), error);
}

/// Check the summary provided contains exactly the changes given.
fn assert_summary_changes(summary: &ChangesSummary, changes: &[RepositoryChange]) {
    let details: Vec<_> = summary.changes.iter().map(|change| change.details()).collect();
    let expected: Vec<_> = changes.iter().map(Change::details).collect();
    assert_eq!(details, expected);
}

/// Expect the invitations of `repo1` to be looked up again when applying a
/// change, returning the same ones seen when loading the state.
fn expect_invitations_lookup(svc: &mut MockSvc, service_state: &ServiceState) {
    let invitations = service_state.repositories[0].invitations.clone();
    svc.expect_list_repository_invitations()
        .withf(|ctx, repo_name| ctx.org == ORG && repo_name == "repo1")
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Ok(invitations.clone()))));
}

/// Expect the invitations of `repo1` to be looked up again when applying a
/// change, failing this time.
fn expect_invitations_lookup_error(svc: &mut MockSvc) {
    svc.expect_list_repository_invitations()
        .withf(|ctx, repo_name| ctx.org == ORG && repo_name == "repo1")
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
}

/// Expect the reads made by a reconciliation to load both states.
fn expect_reconcile_load(svc: &mut MockSvc, service_state: &ServiceState) {
    expect_service_load(svc, service_state);
    expect_config_load(svc, service_state);
}

/// Expect the login of the user provided to be requested.
fn expect_user_login(svc: &mut MockSvc, user: &'static str, login: &'static str) {
    svc.expect_get_user_login()
        .withf(move |ctx, user_name| ctx.org == ORG && user_name == user)
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Ok(login.to_string()))));
}

/// Build a handler using the mocks provided.
fn handler(gh: MockGH, svc: MockSvc) -> Handler {
    Handler::new(Arc::new(gh), Arc::new(svc))
}

/// Build a service state where `repo1` has the collaborator provided, along
/// with an invitation for `other1` and another one without invitee. When
/// `user_invited` is set, `user1` has a pending invitation too.
fn sample_invitations_service_state(collaborator: (&'static str, Role), user_invited: bool) -> ServiceState {
    let mut invitations = vec![
        sample_repository_invitation(8, Some("other1"), &Role::Read),
        sample_repository_invitation(9, None, &Role::Read),
    ];
    if user_invited {
        invitations.push(sample_repository_invitation(
            USER_INVITATION_ID,
            Some("user1"),
            &Role::Write,
        ));
    }
    ServiceState {
        repositories: vec![ServiceRepository {
            collaborators: vec![collaborator],
            invitations,
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    }
}

/// Build a service state valid for the changes summary configurations.
fn sample_service_state() -> ServiceState {
    ServiceState {
        members: vec!["maintainer1"],
        repositories: vec![ServiceRepository::new("repo1")],
        ..Default::default()
    }
}

/// Build a service state where `team1` has access to `repo1`.
fn sample_team_in_repository_service_state() -> ServiceState {
    ServiceState {
        members: vec!["maintainer1"],
        repositories: vec![ServiceRepository {
            teams: vec![("team1", Role::Write)],
            ..ServiceRepository::new("repo1")
        }],
        teams: vec![ServiceTeam::new("team1", "maintainer1")],
        ..Default::default()
    }
}
