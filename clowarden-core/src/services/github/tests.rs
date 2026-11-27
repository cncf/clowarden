use std::{
    collections::BTreeMap,
    future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use anyhow::{Error, format_err};
use mockall::Sequence;
use octorust::{
    ClientError,
    types::{OrgMembershipState, RepositoryInvitation, TeamMembershipRole},
};
use reqwest::StatusCode;
use tokio_util::sync::CancellationToken;

use crate::{
    directory::{DirectoryChange, Team},
    github::{MockGH, RateLimitError, is_cancelled_error, is_rate_limit_error},
    multierror::MultiError,
    services::{BaseRefConfigStatus, Change, ChangeApplied, ChangesSummary, JobContext, ServiceHandler},
};

use super::{
    Handler,
    service::MockSvc,
    state::{Changes, Repository, RepositoryChange, Role, Visibility},
    testing::{
        BASE_REF, HEAD_REF, ORG, ServiceRepository, ServiceState, ServiceTeam, expect_config_file,
        expect_config_file_error, expect_config_load, expect_narrowed_service_load, expect_no_mutations,
        multierror_messages, sample_ctx, sample_head_src, sample_org, sample_repository,
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
    expect_config_load(&mut svc, &sample_service_state());
    svc.expect_get_user_login().never();

    // Get the changes summary
    let result = handler(svc).get_changes_summary(&sample_org(), &sample_head_src(), &job_ctx(gh)).await;

    // Check the configuration was reported as invalid
    assert_eq!(
        result.err().unwrap().to_string(),
        "invalid directory configuration"
    );
}

#[tokio::test]
async fn test_get_changes_summary_fails_when_head_user_is_invalid() {
    // Setup configuration (fetched once per ref thanks to per-job cache)
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, HEAD_REF, 1, HEAD_CONFIG);
    expect_config_file(&mut gh, BASE_REF, 1, BASE_CONFIG);

    // Setup service (metadata loaded once and shared across head and base)
    let mut svc = MockSvc::new();
    let service_state = sample_service_state();
    expect_config_load(&mut svc, &service_state);
    expect_user_login(&mut svc, "member1", "member1");
    expect_user_login(&mut svc, "user1", "User1");

    // Get the changes summary
    let result = handler(svc).get_changes_summary(&sample_org(), &sample_head_src(), &job_ctx(gh)).await;

    // Check the invalid user was reported
    assert_eq!(
        multierror_messages(&result.err().unwrap()),
        vec!["user[user1]: invalid username, should be User1"]
    );
}

#[tokio::test]
async fn test_get_changes_summary_propagates_rate_limit_on_base_read() {
    // Setup configuration where the base read fails with a rate-limit error
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, HEAD_REF, 1, HEAD_CONFIG);
    gh.expect_get_file_content()
        .withf(|src, _| src.ref_ == BASE_REF)
        .times(1)
        .returning(|_, _| {
            Box::pin(future::ready(Err(Error::new(RateLimitError::secondary(
                Some(Duration::from_secs(60)),
                "secondary limit",
            )))))
        });

    // Setup service
    let mut svc = MockSvc::new();
    expect_config_load(&mut svc, &sample_service_state());
    svc.expect_get_user_login().never();

    // Get the changes summary
    let err = handler(svc)
        .get_changes_summary(&sample_org(), &sample_head_src(), &job_ctx(gh))
        .await
        .err()
        .unwrap();

    // Check the rate limit was propagated instead of treating base as invalid
    assert!(is_rate_limit_error(&err));
}

#[tokio::test]
async fn test_get_changes_summary_returns_repository_changes_from_base_to_head() {
    // Setup configuration (fetched once per ref thanks to per-job cache)
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, HEAD_REF, 1, HEAD_CONFIG);
    expect_config_file(&mut gh, BASE_REF, 1, BASE_CONFIG);

    // Setup service, the team member must be validated too
    let mut svc = MockSvc::new();
    let service_state = sample_service_state();
    expect_config_load(&mut svc, &service_state);
    expect_user_login(&mut svc, "member1", "member1");
    expect_user_login(&mut svc, "user1", "user1");
    svc.expect_get_user_login().withf(|_, user_name| user_name == "removed1").never();

    // Get the changes summary
    let summary = handler(svc)
        .get_changes_summary(&sample_org(), &sample_head_src(), &job_ctx(gh))
        .await
        .unwrap();

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
async fn test_get_changes_summary_skips_user_lookup_for_existing_org_members() {
    // Setup configuration adding member1 (already an org member) and user1 (external)
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, HEAD_REF, 1, HEAD_CONFIG);
    expect_config_file(&mut gh, BASE_REF, 1, BASE_CONFIG);

    // Setup service where member1 is already in org_members
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        members: vec!["maintainer1", "member1"],
        repositories: vec![ServiceRepository::new("repo1")],
        ..Default::default()
    };
    expect_config_load(&mut svc, &service_state);
    svc.expect_get_user_login().withf(|_, user_name| user_name == "member1").never();
    expect_user_login(&mut svc, "user1", "user1");

    // Get the changes summary
    let summary = handler(svc)
        .get_changes_summary(&sample_org(), &sample_head_src(), &job_ctx(gh))
        .await
        .unwrap();

    // Check validation succeeded without looking up member1
    assert_eq!(summary.base_ref_config_status, BaseRefConfigStatus::Valid);
}

#[tokio::test]
async fn test_get_changes_summary_tolerates_unreadable_base_reporting_user_problems_as_warnings() {
    // Setup configuration, the base one cannot be fetched
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, HEAD_REF, 1, UNKNOWN_USERS_CONFIG);
    expect_config_file_error(&mut gh, BASE_REF, ERROR);

    // Setup service, all non-member users in the head are looked up
    let mut svc = MockSvc::new();
    let service_state = sample_service_state();
    expect_config_load(&mut svc, &service_state);
    expect_user_login(&mut svc, "unknown1", "Unknown1");
    svc.expect_get_user_login()
        .times(1)
        .withf(|_, user_name| user_name == "unknown2")
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(ERROR)))));

    // Get the changes summary
    let summary = handler(svc)
        .get_changes_summary(&sample_org(), &sample_head_src(), &job_ctx(gh))
        .await
        .unwrap();

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
async fn test_reconcile_adds_new_collaborators_before_refreshing_expired_invitations() {
    // Setup a configuration where an expired collaborator sorts before a brand-new one
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      alpha: write
      zeta: read
",
    );

    // Setup service state where alpha has an expired invitation and zeta is brand-new
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            invitations: vec![sample_expired_invitation(
                USER_INVITATION_ID,
                Some("alpha"),
                &Role::Write,
            )],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Expect zeta to be invited before alpha's expired invitation is deleted and recreated
    let mut seq = Sequence::new();
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "zeta" && *role == Role::Read
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(Some(sample_repository_invitation(
                10,
                Some("zeta"),
                &Role::Read,
            )))))
        });
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| {
            ctx.org == ORG && repo_name == "repo1" && *invitation_id == USER_INVITATION_ID
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "alpha" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(Some(sample_repository_invitation(
                11,
                Some("alpha"),
                &Role::Write,
            )))))
        });

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check brand-new collaborator was applied ahead of the expired refresh
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 2);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "zeta".to_string(), Role::Read),
        None,
    );
    assert_change_applied(
        &outcome.changes_applied[1],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "alpha".to_string(), Role::Write),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_adds_team_before_adding_it_to_repository() {
    // Setup configuration
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
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
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;
    assert!(outcome.error.is_none());
    let changes_applied = outcome.changes_applied;

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
    expect_config_file(&mut gh, BASE_REF, 1, NORMALIZED_CONFIG);

    // Setup service state (including unmanaged repo that must not be inspected)
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
                teams: vec![("ent:team1", Role::Admin), ("my-team", Role::Write)],
                ..ServiceRepository::new("repo1")
            },
            ServiceRepository {
                ignored: true,
                ..ServiceRepository::new("repo1-ghsa-2345-cfgh-jmpq")
            },
            ServiceRepository {
                teams: vec![("ent:team1", Role::Write)],
                visibility: "private",
                ..ServiceRepository::new("repo2")
            },
            ServiceRepository {
                ignored: true,
                ..ServiceRepository::new("unmanaged-repo")
            },
        ],
        teams: vec![
            ServiceTeam {
                ignored: true,
                ..ServiceTeam::new("ent:team1", "maintainer1")
            },
            ServiceTeam {
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
            },
        ],
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected
    expect_no_mutations(&mut svc);

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check no changes were applied
    assert!(outcome.error.is_none());
    assert!(outcome.changes_applied.is_empty());
}

#[tokio::test]
async fn test_reconcile_audits_interrupted_compound_repository_creation_on_rate_limit() {
    // Setup configuration creating a new repository with teams/collaborators
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: new-repo
    external_collaborators:
      user1: write
",
    );

    // Setup service state without new-repo
    let mut svc = MockSvc::new();
    let service_state = ServiceState::default();
    expect_reconcile_load(&mut svc, &service_state);
    svc.expect_add_repository().times(1).returning(|_, _| {
        Box::pin(future::ready(Err(Error::new(RateLimitError::secondary(
            Some(Duration::from_secs(60)),
            "secondary limit mid-create",
        )))))
    });

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check the compound RepositoryAdded attempt was preserved in audit along
    // with the top-level rate-limit error
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_eq!(
        outcome.changes_applied[0].change.details().kind,
        "repository-added"
    );
    assert!(outcome.changes_applied[0].error.is_some());
    assert!(is_rate_limit_error(&outcome.error.unwrap()));
}

#[tokio::test]
async fn test_reconcile_audits_interrupted_expired_reinvite_after_deletion() {
    // Setup a configuration with two collaborators that have expired invitations
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      user1: write
      user2: write
",
    );

    // Setup service state where both users have expired invitations
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            invitations: vec![
                sample_expired_invitation(7, Some("user1"), &Role::Write),
                sample_expired_invitation(8, Some("user2"), &Role::Write),
            ],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup user1 delete succeeding, then user1 add hitting a rate limit; user2 must not run
    let mut seq = Sequence::new();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| ctx.org == ORG && repo_name == "repo1" && *invitation_id == 7)
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user1" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Err(Error::new(RateLimitError::secondary(
                Some(Duration::from_secs(60)),
                "secondary limit",
            )))))
        });
    svc.expect_remove_repository_invitation()
        .withf(|_, repo_name, invitation_id| repo_name == "repo1" && *invitation_id == 8)
        .never();
    svc.expect_add_repository_collaborator()
        .withf(|_, repo_name, user_name, _| repo_name == "repo1" && user_name == "user2")
        .never();

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check partial deletion was preserved in audit and reconciliation stopped on rate limit
    assert!(is_rate_limit_error(&outcome.error.unwrap()));
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user1".to_string(), Role::Write),
        Some("github rate limit exceeded (secondary): secondary limit"),
    );
}

#[tokio::test]
async fn test_reconcile_continues_after_failed_change() {
    // Setup configuration
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
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
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;
    assert!(outcome.error.is_none());
    let changes_applied = outcome.changes_applied;

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
async fn test_reconcile_deletes_all_expired_invitations_before_reinviting() {
    // Setup a configuration requiring user1 as an external collaborator
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      user1: write
",
    );

    // Setup service state where user1 has two expired invitations on the same repo
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            invitations: vec![
                sample_expired_invitation(7, Some("user1"), &Role::Read),
                sample_expired_invitation(8, Some("user1"), &Role::Write),
            ],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Expect both expired invitations to be deleted before creating the fresh invitation
    let mut seq = Sequence::new();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| ctx.org == ORG && repo_name == "repo1" && *invitation_id == 7)
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| ctx.org == ORG && repo_name == "repo1" && *invitation_id == 8)
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user1" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(Some(sample_repository_invitation(
                9,
                Some("user1"),
                &Role::Write,
            )))))
        });

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check user1 was re-invited after both expired invitations were deleted
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user1".to_string(), Role::Write),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_does_not_reinvite_when_deleting_expired_invitation_fails() {
    // Setup a configuration requiring user1 as an external collaborator
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      user1: write
",
    );

    // Setup service state where user1 only has an expired invitation
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            invitations: vec![sample_expired_invitation(
                USER_INVITATION_ID,
                Some("user1"),
                &Role::Write,
            )],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup delete failure; add_repository_collaborator must not be called
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| {
            ctx.org == ORG && repo_name == "repo1" && *invitation_id == USER_INVITATION_ID
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(ERROR)))));
    svc.expect_add_repository_collaborator().never();

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check failure was recorded without issuing a no-op collaborator add
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user1".to_string(), Role::Write),
        Some(ERROR),
    );
}

#[tokio::test]
async fn test_reconcile_does_not_skip_expired_reinvites_after_non_refresh_422() {
    // Setup a configuration with a brand-new outside collaborator, an org member
    // with an expired invite, and an outside collaborator with an expired invite
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      exp1: write
      member1: read
      new1: write
",
    );

    // Setup service state where member1 is an org member and both member1 and exp1 have expired invites
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        members: vec!["member1"],
        repositories: vec![ServiceRepository {
            invitations: vec![
                sample_expired_invitation(1, Some("exp1"), &Role::Write),
                sample_expired_invitation(2, Some("member1"), &Role::Read),
            ],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup member1 (org-member expired refresh) and new1 (brand-new add) both
    // returning 422; exp1 must still be refreshed afterwards
    let mut seq = Sequence::new();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| ctx.org == ORG && repo_name == "repo1" && *invitation_id == 2)
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "member1" && *role == Role::Read
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Err(Error::new(ClientError::HttpError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                error: "cannot assign member permission of pull".to_string(),
            }))))
        });
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "new1" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Err(Error::new(ClientError::HttpError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                error: "validation failed".to_string(),
            }))))
        });
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| ctx.org == ORG && repo_name == "repo1" && *invitation_id == 1)
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "exp1" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(Some(sample_repository_invitation(
                10,
                Some("exp1"),
                &Role::Write,
            )))))
        });

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check member1 and new1 failed with 422 while exp1 was still refreshed
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 3);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "member1".to_string(), Role::Read),
        Some("HTTP Error. Code: 422 Unprocessable Entity, message: cannot assign member permission of pull"),
    );
    assert_change_applied(
        &outcome.changes_applied[1],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "new1".to_string(), Role::Write),
        Some("HTTP Error. Code: 422 Unprocessable Entity, message: validation failed"),
    );
    assert_change_applied(
        &outcome.changes_applied[2],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "exp1".to_string(), Role::Write),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_fails_fast_without_scanning_service_when_config_cannot_be_read() {
    // Setup configuration
    let mut gh = MockGH::new();
    expect_config_file_error(&mut gh, BASE_REF, ERROR);

    // Setup service metadata only; teams and repo details must NOT be scanned
    let mut svc = MockSvc::new();
    expect_config_load(&mut svc, &sample_service_state());
    svc.expect_list_teams().never();
    svc.expect_list_repository_collaborators().never();
    svc.expect_list_repository_invitations().never();
    svc.expect_list_repository_teams().never();
    expect_no_mutations(&mut svc);

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check the desired state was reported as unavailable
    assert!(outcome.changes_applied.is_empty());
    assert_eq!(
        outcome.error.unwrap().to_string(),
        "error getting desired state from configuration"
    );
}

#[tokio::test]
async fn test_reconcile_fails_fast_without_scanning_service_when_config_is_invalid() {
    // Setup a configuration using a team that does not exist
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    teams:
      unknown: write
",
    );

    // Setup service metadata only; teams and repo details must NOT be scanned
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository::new("repo1")],
        ..Default::default()
    };
    expect_config_load(&mut svc, &service_state);
    svc.expect_list_teams().never();
    svc.expect_list_repository_collaborators().never();
    expect_no_mutations(&mut svc);

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check the configuration problem was reported
    let err = outcome.error.unwrap();
    assert!(outcome.changes_applied.is_empty());
    assert_eq!(err.to_string(), "error getting desired state from configuration");
    assert_eq!(
        multierror_messages(&err),
        vec!["repo[repo1]: team unknown does not exist in directory"]
    );
}

#[tokio::test]
async fn test_reconcile_fails_when_reinvite_does_not_create_fresh_invitation() {
    // Setup a configuration requiring three external collaborators with expired invites
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      user1: write
      user2: write
      user3: write
",
    );

    // Setup service state where none are org members and all three have expired invites
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            invitations: vec![
                sample_expired_invitation(7, Some("user1"), &Role::Write),
                sample_expired_invitation(8, Some("user2"), &Role::Write),
                sample_expired_invitation(9, Some("user3"), &Role::Write),
            ],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup user1 receiving 204 No Content, user2 receiving the stale deleted id,
    // and user3 receiving a new id marked expired
    let mut seq1 = Sequence::new();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| ctx.org == ORG && repo_name == "repo1" && *invitation_id == 7)
        .times(1)
        .in_sequence(&mut seq1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user1" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(None))));

    let mut seq2 = Sequence::new();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| ctx.org == ORG && repo_name == "repo1" && *invitation_id == 8)
        .times(1)
        .in_sequence(&mut seq2)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user2" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq2)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(Some(sample_repository_invitation(
                8,
                Some("user2"),
                &Role::Write,
            )))))
        });

    let mut seq3 = Sequence::new();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| ctx.org == ORG && repo_name == "repo1" && *invitation_id == 9)
        .times(1)
        .in_sequence(&mut seq3)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user3" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq3)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(Some(sample_expired_invitation(
                99,
                Some("user3"),
                &Role::Write,
            )))))
        });

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check all three collaborators were recorded as errors rather than silently applied
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 3);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user1".to_string(), Role::Write),
        Some("github did not create a fresh invitation for collaborator user1 in repo1"),
    );
    assert_change_applied(
        &outcome.changes_applied[1],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user2".to_string(), Role::Write),
        Some("github did not create a fresh invitation for collaborator user2 in repo1"),
    );
    assert_change_applied(
        &outcome.changes_applied[2],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user3".to_string(), Role::Write),
        Some("github did not create a fresh invitation for collaborator user3 in repo1"),
    );
}

#[tokio::test]
async fn test_reconcile_fails_without_mutations_when_service_state_cannot_be_read() {
    // Setup valid configuration referencing repo1
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
",
    );

    // Setup service state where repo1's teams fail to load
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository::new("repo1")],
        ..Default::default()
    };
    expect_config_load(&mut svc, &service_state);
    svc.expect_list_teams().times(1).returning(|_| Box::pin(future::ready(Ok(vec![]))));
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
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check the actual state was reported as unavailable
    assert!(outcome.changes_applied.is_empty());
    assert_eq!(
        outcome.error.unwrap().to_string(),
        "error getting actual state from service"
    );
}

#[tokio::test]
async fn test_reconcile_falls_back_to_collaborator_removal_when_invitation_returns_404() {
    // Setup a configuration that drops a collaborator
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 1, INVITATIONS_CONFIG);

    // Setup service state where user1 had a pending invitation at scan time
    let mut svc = MockSvc::new();
    let service_state = sample_invitations_service_state(("other1", Role::Read), true);
    expect_reconcile_load(&mut svc, &service_state);

    // Setup invitation deletion returning 404 (accepted mid-run) then collaborator removal succeeding
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| {
            ctx.org == ORG && repo_name == "repo1" && *invitation_id == USER_INVITATION_ID
        })
        .times(1)
        .returning(|_, _, _| {
            Box::pin(future::ready(Err(Error::new(ClientError::HttpError {
                status: StatusCode::NOT_FOUND,
                error: "Not Found".to_string(),
            }))))
        });
    svc.expect_remove_repository_collaborator()
        .withf(|ctx, repo_name, user_name| ctx.org == ORG && repo_name == "repo1" && user_name == "user1")
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check fallback succeeded cleanly
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorRemoved("repo1".to_string(), "user1".to_string()),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_falls_back_to_collaborator_role_update_when_invitation_returns_404() {
    // Setup a configuration that updates a collaborator's role
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 1, INVITATIONS_ROLE_UPDATED_CONFIG);

    // Setup service state where user1 had a pending invitation at scan time
    let mut svc = MockSvc::new();
    let service_state = sample_invitations_service_state(("other1", Role::Read), true);
    expect_reconcile_load(&mut svc, &service_state);

    // Setup invitation update returning 404 then collaborator role update succeeding
    svc.expect_update_repository_invitation()
        .withf(|ctx, repo_name, invitation_id, role| {
            ctx.org == ORG
                && repo_name == "repo1"
                && *invitation_id == USER_INVITATION_ID
                && *role == Role::Admin
        })
        .times(1)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Err(Error::new(ClientError::HttpError {
                status: StatusCode::NOT_FOUND,
                error: "Not Found".to_string(),
            }))))
        });
    svc.expect_update_repository_collaborator_role()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user1" && *role == Role::Admin
        })
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(()))));

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check fallback succeeded cleanly
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorRoleUpdated("repo1".to_string(), "user1".to_string(), Role::Admin),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_ignores_expired_invitation_when_removing_or_updating_active_collaborator() {
    // Setup a configuration removing user1 and changing user2's role
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      user2: admin
",
    );

    // Setup service state where both active collaborators also have stale expired invitations
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            collaborators: vec![("user1", Role::Write), ("user2", Role::Write)],
            invitations: vec![
                sample_expired_invitation(7, Some("user1"), &Role::Write),
                sample_expired_invitation(8, Some("user2"), &Role::Write),
            ],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Expect direct collaborator mutations rather than invitation endpoints
    svc.expect_remove_repository_collaborator()
        .withf(|ctx, repo_name, user_name| ctx.org == ORG && repo_name == "repo1" && user_name == "user1")
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_update_repository_collaborator_role()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user2" && *role == Role::Admin
        })
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_remove_repository_invitation().never();
    svc.expect_update_repository_invitation().never();

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check both active collaborator changes used collaborator endpoints
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 2);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorRemoved("repo1".to_string(), "user1".to_string()),
        None,
    );
    assert_change_applied(
        &outcome.changes_applied[1],
        &RepositoryChange::CollaboratorRoleUpdated("repo1".to_string(), "user2".to_string(), Role::Admin),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_omits_interrupted_expired_reinvite_when_deletion_is_rate_limited() {
    // Setup a configuration requiring user1 as an external collaborator
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      user1: write
",
    );

    // Setup service state where user1 has an expired invitation
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            invitations: vec![sample_expired_invitation(
                USER_INVITATION_ID,
                Some("user1"),
                &Role::Write,
            )],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup the initial delete itself hitting a rate limit before any mutation occurs
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| {
            ctx.org == ORG && repo_name == "repo1" && *invitation_id == USER_INVITATION_ID
        })
        .times(1)
        .returning(|_, _, _| {
            Box::pin(future::ready(Err(Error::new(RateLimitError::secondary(
                Some(Duration::from_secs(60)),
                "secondary limit",
            )))))
        });
    svc.expect_add_repository_collaborator().never();

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check no partial mutation was recorded and rate-limit error was surfaced
    assert!(is_rate_limit_error(&outcome.error.unwrap()));
    assert!(outcome.changes_applied.is_empty());
}

#[tokio::test]
async fn test_reconcile_reinvites_collaborator_when_expired_invitation_returns_404() {
    // Setup a configuration requiring user1 as an external collaborator
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      user1: write
",
    );

    // Setup service state where user1 had an expired invitation at scan time
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            invitations: vec![sample_expired_invitation(
                USER_INVITATION_ID,
                Some("user1"),
                &Role::Write,
            )],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup invitation deletion returning 404 (already gone) followed by fresh invite
    let mut seq = Sequence::new();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| {
            ctx.org == ORG && repo_name == "repo1" && *invitation_id == USER_INVITATION_ID
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| {
            Box::pin(future::ready(Err(Error::new(ClientError::HttpError {
                status: StatusCode::NOT_FOUND,
                error: "Not Found".to_string(),
            }))))
        });
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user1" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(Some(sample_repository_invitation(
                USER_INVITATION_ID + 1,
                Some("user1"),
                &Role::Write,
            )))))
        });

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check collaborator was still re-invited after 404 on expired invitation delete
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user1".to_string(), Role::Write),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_reinvites_collaborator_with_expired_repository_invitation() {
    // Setup a configuration requiring user1 as an external collaborator
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      user1: write
",
    );

    // Setup service state where user1 only has an expired invitation
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            invitations: vec![sample_expired_invitation(
                USER_INVITATION_ID,
                Some("user1"),
                &Role::Write,
            )],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Expect the expired invitation to be deleted before creating a fresh one
    let mut seq = Sequence::new();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| {
            ctx.org == ORG && repo_name == "repo1" && *invitation_id == USER_INVITATION_ID
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user1" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(Some(sample_repository_invitation(
                USER_INVITATION_ID + 1,
                Some("user1"),
                &Role::Write,
            )))))
        });
    svc.expect_update_repository_invitation().never();

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check user1 was re-invited
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user1".to_string(), Role::Write),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_reinvites_org_member_with_expired_invitation_accepting_no_content() {
    // Setup a configuration requiring user1 as a collaborator
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      user1: write
",
    );

    // Setup service state where user1 joined the org after their old invitation expired
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        members: vec!["user1"],
        repositories: vec![ServiceRepository {
            invitations: vec![sample_expired_invitation(
                USER_INVITATION_ID,
                Some("user1"),
                &Role::Write,
            )],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Expect expired invitation deletion followed by direct 204 org-member grant
    let mut seq = Sequence::new();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| {
            ctx.org == ORG && repo_name == "repo1" && *invitation_id == USER_INVITATION_ID
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user1" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(None))));

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check 204 No Content succeeded for the organization member
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user1".to_string(), Role::Write),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_rejects_case_mismatched_expired_invitation_without_mutating() {
    // Setup a configuration whose username casing differs from GitHub's canonical login
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      user1: write
",
    );

    // Setup service state where GitHub has an expired invitation for User1
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![ServiceRepository {
            invitations: vec![sample_expired_invitation(
                USER_INVITATION_ID,
                Some("User1"),
                &Role::Write,
            )],
            ..ServiceRepository::new("repo1")
        }],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);
    expect_no_mutations(&mut svc);

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check casing mismatch was reported without deleting or re-inviting
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user1".to_string(), Role::Write),
        Some("collaborator user1 in repo1 has invalid username casing, should be User1"),
    );
}

#[tokio::test]
async fn test_reconcile_removes_collaborator_without_pending_invitation() {
    // Setup a configuration that drops a collaborator
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 1, INVITATIONS_CONFIG);

    // Setup service state, with invitations that do not belong to the user
    let mut svc = MockSvc::new();
    let service_state = sample_invitations_service_state(("user1", Role::Write), false);
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected (no second list_repository_invitations call)
    svc.expect_remove_repository_collaborator()
        .withf(|ctx, repo_name, user_name| ctx.org == ORG && repo_name == "repo1" && user_name == "user1")
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_remove_repository_invitation().never();

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check the change applied
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
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
        1,
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
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;
    assert!(outcome.error.is_none());
    let changes_applied = outcome.changes_applied;

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
        1,
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
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;
    assert!(outcome.error.is_none());
    let changes_applied = outcome.changes_applied;

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
    expect_config_file(&mut gh, BASE_REF, 1, INVITATIONS_CONFIG);

    // Setup service state, where the user has a pending invitation
    let mut svc = MockSvc::new();
    let service_state = sample_invitations_service_state(("other1", Role::Read), true);
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| {
            ctx.org == ORG && repo_name == "repo1" && *invitation_id == USER_INVITATION_ID
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_remove_repository_collaborator().never();

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check the change applied
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
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
        1,
        "
teams:
  - name: team1
    maintainers:
      - maintainer1
repositories:
  - name: repo1
",
    );

    // Setup service state (team2 is removed so its details must not be inspected)
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        members: vec!["maintainer1"],
        repositories: vec![ServiceRepository {
            teams: vec![("team1", Role::Write), ("team2", Role::Write)],
            ..ServiceRepository::new("repo1")
        }],
        teams: vec![
            ServiceTeam::new("team1", "maintainer1"),
            ServiceTeam {
                ignored: true,
                ..ServiceTeam::new("team2", "maintainer1")
            },
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
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;
    assert!(outcome.error.is_none());
    let changes_applied = outcome.changes_applied;

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
        1,
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
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check the change applied
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::TeamRemoved("repo1".to_string(), "team1".to_string()),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_removes_team_from_repository_when_team_removal_fails() {
    // Setup a configuration that drops the team
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 1, TEAM_DROPPED_CONFIG);

    // Setup service state (team1 is removed so its details must not be inspected)
    let mut svc = MockSvc::new();
    let service_state = sample_dropped_team_in_repository_service_state();
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
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;
    assert!(outcome.error.is_none());
    let changes_applied = outcome.changes_applied;

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
async fn test_reconcile_replaces_expired_invitation_and_converges_on_next_sweep() {
    // Setup configuration read across two consecutive reconciliation sweeps
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
      user1: write
",
    );
    let gh = Arc::new(gh);

    // Setup stateful repository invitations enforcing real GitHub behavior
    let invitations = Arc::new(Mutex::new(vec![sample_expired_invitation(
        USER_INVITATION_ID,
        Some("user1"),
        &Role::Write,
    )]));
    let handler = handler(setup_stateful_invitation_svc(invitations.clone(), 2));

    // Run first sweep and verify the expired invitation was replaced by a fresh one
    let first_outcome = handler
        .reconcile(
            &sample_org(),
            &JobContext::new(gh.clone(), CancellationToken::new()),
        )
        .await;
    assert!(first_outcome.error.is_none());
    assert_eq!(first_outcome.changes_applied.len(), 1);
    assert_change_applied(
        &first_outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "user1".to_string(), Role::Write),
        None,
    );
    let current_invitations: Vec<RepositoryInvitation> =
        invitations.lock().expect("invitations lock not to be poisoned").clone();
    assert_eq!(current_invitations.len(), 1);
    assert_eq!(current_invitations[0].id, USER_INVITATION_ID + 1);
    assert!(!current_invitations[0].expired);
    assert_eq!(
        current_invitations[0].invitee.as_ref().map(|user| user.login.as_str()),
        Some("user1")
    );

    // Run second sweep and verify no duplicate changes are emitted
    let second_outcome =
        handler.reconcile(&sample_org(), &JobContext::new(gh, CancellationToken::new())).await;
    assert!(second_outcome.error.is_none());
    assert!(second_outcome.changes_applied.is_empty());
}

#[tokio::test]
async fn test_reconcile_skips_remaining_expired_reinvites_on_repo_after_422() {
    // Setup a configuration with two expired collaborators in repo1 and one in repo2
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
teams: []
repositories:
  - name: repo1
    external_collaborators:
      exp1: write
      exp2: write
  - name: repo2
    external_collaborators:
      exp3: write
",
    );

    // Setup service state where all three users have expired invitations
    let mut svc = MockSvc::new();
    let service_state = ServiceState {
        repositories: vec![
            ServiceRepository {
                invitations: vec![
                    sample_expired_invitation(1, Some("exp1"), &Role::Write),
                    sample_expired_invitation(2, Some("exp2"), &Role::Write),
                ],
                ..ServiceRepository::new("repo1")
            },
            ServiceRepository {
                invitations: vec![sample_expired_invitation(3, Some("exp3"), &Role::Write)],
                ..ServiceRepository::new("repo2")
            },
        ],
        ..Default::default()
    };
    expect_reconcile_load(&mut svc, &service_state);

    // Setup repo1 exp1 failing with 422, repo1 exp2 skipped, and repo2 exp3 succeeding
    let mut seq1 = Sequence::new();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| ctx.org == ORG && repo_name == "repo1" && *invitation_id == 1)
        .times(1)
        .in_sequence(&mut seq1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "exp1" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq1)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Err(Error::new(ClientError::HttpError {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                error: "validation failed".to_string(),
            }))))
        });

    svc.expect_remove_repository_invitation()
        .withf(|_, repo_name, invitation_id| repo_name == "repo1" && *invitation_id == 2)
        .never();
    svc.expect_add_repository_collaborator()
        .withf(|_, repo_name, user_name, _| repo_name == "repo1" && user_name == "exp2")
        .never();

    let mut seq2 = Sequence::new();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| ctx.org == ORG && repo_name == "repo2" && *invitation_id == 3)
        .times(1)
        .in_sequence(&mut seq2)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo2" && user_name == "exp3" && *role == Role::Write
        })
        .times(1)
        .in_sequence(&mut seq2)
        .returning(|_, _, _, _| {
            Box::pin(future::ready(Ok(Some(sample_repository_invitation(
                4,
                Some("exp3"),
                &Role::Write,
            )))))
        });

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check repo1 exp1 failed, repo1 exp2 was skipped without deleting its invite, and repo2 exp3 succeeded
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 2);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorAdded("repo1".to_string(), "exp1".to_string(), Role::Write),
        Some("HTTP Error. Code: 422 Unprocessable Entity, message: validation failed"),
    );
    assert_change_applied(
        &outcome.changes_applied[1],
        &RepositoryChange::CollaboratorAdded("repo2".to_string(), "exp3".to_string(), Role::Write),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_skips_repository_team_removal_for_removed_team() {
    // Setup a configuration that drops the team
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 1, TEAM_DROPPED_CONFIG);

    // Setup service state (team1 is removed so its details must not be inspected)
    let mut svc = MockSvc::new();
    let service_state = sample_dropped_team_in_repository_service_state();
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected
    svc.expect_remove_team()
        .withf(|ctx, team_name| ctx.org == ORG && team_name == "team1")
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));
    svc.expect_remove_repository_team().never();

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check only the team removal was recorded
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &DirectoryChange::TeamRemoved("team1".to_string()),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_stops_early_on_cancellation_preserving_applied_changes() {
    // Setup a configuration with multiple repository changes
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
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

    // Cancel after the first mutation succeeds
    let cancel_token = CancellationToken::new();
    let cancel_after_first = cancel_token.clone();
    svc.expect_add_repository_team().times(1).returning(move |_, _, _, _| {
        cancel_after_first.cancel();
        Box::pin(future::ready(Ok(())))
    });
    svc.expect_add_repository_collaborator().never();
    svc.expect_update_repository_visibility().never();

    // Run the reconciliation
    let ctx = JobContext::new(Arc::new(gh), cancel_token);
    let outcome = handler(svc).reconcile(&sample_org(), &ctx).await;

    // Check first change is preserved and cancellation error is reported
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::TeamAdded("repo1".to_string(), "team1".to_string(), Role::Write),
        None,
    );
    assert!(is_cancelled_error(&outcome.error.unwrap()));
}

#[tokio::test]
async fn test_reconcile_stops_early_on_rate_limit_preserving_applied_changes() {
    // Setup a configuration with multiple repository changes
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
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

    // First mutation succeeds, second hits rate limit, third must not run
    svc.expect_add_repository_team()
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_add_repository_collaborator().times(1).returning(|_, _, _, _| {
        Box::pin(future::ready(Err(Error::new(RateLimitError::secondary(
            Some(Duration::from_secs(60)),
            "secondary limit",
        )))))
    });
    svc.expect_update_repository_visibility().never();

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check applied change survived and rate limit error is surfaced
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::TeamAdded("repo1".to_string(), "team1".to_string(), Role::Write),
        None,
    );
    assert!(is_rate_limit_error(&outcome.error.unwrap()));
}

#[tokio::test]
async fn test_reconcile_updates_collaborator_role_without_pending_invitation() {
    // Setup a configuration that updates a collaborator's role
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 1, INVITATIONS_ROLE_UPDATED_CONFIG);

    // Setup service state, with invitations that do not belong to the user
    let mut svc = MockSvc::new();
    let service_state = sample_invitations_service_state(("user1", Role::Write), false);
    expect_reconcile_load(&mut svc, &service_state);

    // Setup mutations expected
    svc.expect_update_repository_collaborator_role()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user1" && *role == Role::Admin
        })
        .times(1)
        .returning(|_, _, _, _| Box::pin(future::ready(Ok(()))));
    svc.expect_update_repository_invitation().never();

    // Run the reconciliation
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check the change applied
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
        &RepositoryChange::CollaboratorRoleUpdated("repo1".to_string(), "user1".to_string(), Role::Admin),
        None,
    );
}

#[tokio::test]
async fn test_reconcile_updates_pending_invitation_of_updated_collaborator() {
    // Setup a configuration that updates a collaborator's role
    let mut gh = MockGH::new();
    expect_config_file(&mut gh, BASE_REF, 1, INVITATIONS_ROLE_UPDATED_CONFIG);

    // Setup service state, where the user has a pending invitation
    let mut svc = MockSvc::new();
    let service_state = sample_invitations_service_state(("other1", Role::Read), true);
    expect_reconcile_load(&mut svc, &service_state);

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
    let outcome = handler(svc).reconcile(&sample_org(), &job_ctx(gh)).await;

    // Check the change applied
    assert!(outcome.error.is_none());
    assert_eq!(outcome.changes_applied.len(), 1);
    assert_change_applied(
        &outcome.changes_applied[0],
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
    let result = handler(svc).validate_users(&sample_ctx(), &changes, &[]).await;

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
    let err = handler(svc).validate_users(&sample_ctx(), &changes, &[]).await.unwrap_err();

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
    let err = handler(svc).validate_users(&sample_ctx(), &changes, &[]).await.unwrap_err();

    // Check the user was reported
    assert_eq!(
        multierror_messages(&err),
        vec!["user[member1]: invalid username, should be Member1"]
    );
}

#[tokio::test]
async fn test_validate_users_preserves_non_404_client_error_and_treats_404_as_config_error() {
    // Setup two users: missing-user returns 404 (config mistake) and
    // server-err-user returns 500 (operational ClientError)
    let changes = Changes {
        repositories: vec![
            RepositoryChange::CollaboratorAdded("repo1".to_string(), "missing-user".to_string(), Role::Read),
            RepositoryChange::CollaboratorAdded(
                "repo1".to_string(),
                "server-err-user".to_string(),
                Role::Read,
            ),
        ],
        ..Default::default()
    };
    let mut svc = MockSvc::new();
    svc.expect_get_user_login()
        .withf(|_, user_name| user_name == "missing-user")
        .times(1)
        .returning(|_, _| {
            Box::pin(future::ready(Err(Error::new(octorust::ClientError::HttpError {
                status: reqwest::StatusCode::NOT_FOUND,
                error: "Not Found".to_string(),
            }))))
        });
    svc.expect_get_user_login()
        .withf(|_, user_name| user_name == "server-err-user")
        .times(1)
        .returning(|_, _| {
            Box::pin(future::ready(Err(Error::new(octorust::ClientError::HttpError {
                status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                error: "Internal Server Error".to_string(),
            }))))
        });

    // Validate users
    let err = handler(svc).validate_users(&sample_ctx(), &changes, &[]).await.unwrap_err();
    let merr = err.downcast_ref::<MultiError>().unwrap();
    let errors = merr.errors();

    // Check 404 lost ClientError (plain config error) while 500 preserved it
    assert_eq!(errors.len(), 2);
    assert!(!errors[0].chain().any(|cause| cause.downcast_ref::<octorust::ClientError>().is_some()));
    assert!(errors[1].chain().any(|cause| cause.downcast_ref::<octorust::ClientError>().is_some()));
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
    let err = handler(svc).validate_users(&sample_ctx(), &changes, &[]).await.unwrap_err();

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
    let result = handler(svc).validate_users(&sample_ctx(), &changes, &[]).await;

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

/// Expect the reads made by a reconciliation to load metadata and narrowed
/// actual state without redundant org-level calls.
fn expect_reconcile_load(svc: &mut MockSvc, service_state: &ServiceState) {
    expect_config_load(svc, service_state);
    expect_narrowed_service_load(svc, service_state);
}

/// Expect the login of the user provided to be requested.
fn expect_user_login(svc: &mut MockSvc, user: &'static str, login: &'static str) {
    svc.expect_get_user_login()
        .withf(move |ctx, user_name| ctx.org == ORG && user_name == user)
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Ok(login.to_string()))));
}

/// Build a handler using the service mock provided.
fn handler(svc: MockSvc) -> Handler {
    Handler::new(Arc::new(svc))
}

/// Build a per-job context wrapping the GitHub mock provided.
fn job_ctx(gh: MockGH) -> JobContext {
    JobContext::new(Arc::new(gh), CancellationToken::new())
}

/// Build a service state where `team1` is present in GitHub and has access to
/// `repo1`, and will be dropped by configuration (so its details are ignored).
fn sample_dropped_team_in_repository_service_state() -> ServiceState {
    ServiceState {
        members: vec!["maintainer1"],
        repositories: vec![ServiceRepository {
            teams: vec![("team1", Role::Write)],
            ..ServiceRepository::new("repo1")
        }],
        teams: vec![ServiceTeam {
            ignored: true,
            ..ServiceTeam::new("team1", "maintainer1")
        }],
        ..Default::default()
    }
}

/// Build an expired repository invitation with the role provided.
fn sample_expired_invitation(id: i64, invitee: Option<&str>, role: &Role) -> RepositoryInvitation {
    let mut invitation = sample_repository_invitation(id, invitee, role);
    invitation.expired = true;
    invitation
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

/// Build a [`MockSvc`] backed by shared invitation state for `repo1` that
/// models GitHub's no-op behavior whenever an invitation (active or expired)
/// already exists for the user.
fn setup_stateful_invitation_svc(
    invitations: Arc<Mutex<Vec<RepositoryInvitation>>>,
    sweeps: usize,
) -> MockSvc {
    let next_id = Arc::new(AtomicI64::new(USER_INVITATION_ID + 1));
    let mut svc = MockSvc::new();

    // Setup per-sweep read expectations
    svc.expect_list_org_admins()
        .withf(|ctx| ctx.org == ORG)
        .times(sweeps)
        .returning(|_| Box::pin(future::ready(Ok(vec![]))));
    svc.expect_list_org_members()
        .withf(|ctx| ctx.org == ORG)
        .times(sweeps)
        .returning(|_| Box::pin(future::ready(Ok(vec![]))));
    svc.expect_list_repositories().withf(|ctx| ctx.org == ORG).times(sweeps).returning(|_| {
        Box::pin(future::ready(Ok(vec![sample_repository(
            "repo1", "public", false,
        )])))
    });
    svc.expect_list_teams()
        .withf(|ctx| ctx.org == ORG)
        .times(sweeps)
        .returning(|_| Box::pin(future::ready(Ok(vec![]))));
    svc.expect_list_repository_collaborators()
        .withf(|ctx, repo_name| ctx.org == ORG && repo_name == "repo1")
        .times(sweeps)
        .returning(|_, _| Box::pin(future::ready(Ok(vec![]))));
    svc.expect_list_repository_teams()
        .withf(|ctx, repo_name| ctx.org == ORG && repo_name == "repo1")
        .times(sweeps)
        .returning(|_, _| Box::pin(future::ready(Ok(vec![]))));

    // Read live invitation list on each sweep
    let list_invitations = invitations.clone();
    svc.expect_list_repository_invitations()
        .withf(|ctx, repo_name| ctx.org == ORG && repo_name == "repo1")
        .times(sweeps)
        .returning(move |_, _| {
            let snapshot = list_invitations.lock().expect("invitations lock not to be poisoned").clone();
            Box::pin(future::ready(Ok(snapshot)))
        });

    // Mutate invitation state on delete and enforce GitHub's no-op rule on add
    let remove_invitations = invitations.clone();
    svc.expect_remove_repository_invitation()
        .withf(|ctx, repo_name, invitation_id| {
            ctx.org == ORG && repo_name == "repo1" && *invitation_id == USER_INVITATION_ID
        })
        .times(1)
        .returning(move |_, _, invitation_id| {
            let mut guard = remove_invitations.lock().expect("invitations lock not to be poisoned");
            guard.retain(|invitation| invitation.id != invitation_id);
            Box::pin(future::ready(Ok(())))
        });
    svc.expect_add_repository_collaborator()
        .withf(|ctx, repo_name, user_name, role| {
            ctx.org == ORG && repo_name == "repo1" && user_name == "user1" && *role == Role::Write
        })
        .times(1)
        .returning(move |_, _, user_name, role| {
            let mut guard = invitations.lock().expect("invitations lock not to be poisoned");
            let has_existing = guard.iter().any(|invitation| {
                invitation
                    .invitee
                    .as_ref()
                    .is_some_and(|invitee| invitee.login.eq_ignore_ascii_case(user_name))
            });
            if has_existing {
                return Box::pin(future::ready(Ok(None)));
            }
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let fresh = sample_repository_invitation(id, Some(user_name), role);
            guard.push(fresh.clone());
            Box::pin(future::ready(Ok(Some(fresh))))
        });

    svc
}
