use std::{future, sync::Arc};

use anyhow::format_err;
use serde_json::{Value, json};

use crate::{
    cfg::{Legacy, Organization},
    github::{MockGH, Source},
    multierror::format_error,
    services::{BaseRefConfigStatus, Change},
};

use super::{Directory, DirectoryChange, Team, User};

const INSTALLATION_ID: i64 = 1;
const PEOPLE_PATH: &str = "config/people.json";
const PERMISSIONS_PATH: &str = "config/permissions.yaml";
const VALID_PERMISSIONS: &str = "teams: [{name: t, maintainers: [a]}]";

#[test]
fn test_details_team_variants_match_audit_contract() {
    // Setup cases for each of the team changes
    let team1 = "team1".to_string();
    let user1 = "user1".to_string();
    let team_and_user = json!({ "team_name": "team1", "user_name": "user1" });
    let cases: Vec<(DirectoryChange, &str, Value)> = vec![
        (
            DirectoryChange::TeamAdded(sample_team()),
            "team-added",
            json!({ "team": { "name": "team1", "maintainers": ["user1"], "members": ["user2"] } }),
        ),
        (
            DirectoryChange::TeamRemoved(team1.clone()),
            "team-removed",
            json!({ "team_name": "team1" }),
        ),
        (
            DirectoryChange::TeamMaintainerAdded(team1.clone(), user1.clone()),
            "team-maintainer-added",
            team_and_user.clone(),
        ),
        (
            DirectoryChange::TeamMaintainerRemoved(team1.clone(), user1.clone()),
            "team-maintainer-removed",
            team_and_user.clone(),
        ),
        (
            DirectoryChange::TeamMemberAdded(team1.clone(), user1.clone()),
            "team-member-added",
            team_and_user.clone(),
        ),
        (
            DirectoryChange::TeamMemberRemoved(team1, user1),
            "team-member-removed",
            team_and_user,
        ),
    ];

    // Check the kind and extra fields the audit UI relies on
    for (change, kind, extra) in cases {
        let details = change.details();
        assert_eq!(details.kind, kind);
        assert_eq!(details.extra, extra);
    }
}

#[test]
fn test_diff_empty_when_directories_are_identical() {
    // Setup a directory with a team and a user
    let dir = Directory {
        teams: vec![sample_team()],
        users: vec![User {
            full_name: "user1".to_string(),
            ..Default::default()
        }],
    };

    // Check no changes are detected
    assert_eq!(dir.diff(&dir.clone()), vec![]);
}

#[test]
fn test_diff_empty_when_only_team_display_name_differs() {
    // Setup the same team as read from the service and from the configuration
    let team_service = Team {
        display_name: Some("Team 1".to_string()),
        ..sample_team()
    };
    let dir1 = Directory {
        teams: vec![team_service],
        ..Default::default()
    };
    let dir2 = Directory {
        teams: vec![sample_team()],
        ..Default::default()
    };

    // Check no changes are detected
    assert_eq!(dir1.diff(&dir2), vec![]);
}

#[test]
fn test_diff_empty_when_team_users_order_differs() {
    // Setup the same team listing its users in a different order
    let team1 = Team {
        name: "team1".to_string(),
        maintainers: vec!["user1".to_string(), "user2".to_string()],
        members: vec!["user3".to_string(), "user4".to_string()],
        ..Default::default()
    };
    let team1_reordered = Team {
        maintainers: vec!["user2".to_string(), "user1".to_string()],
        members: vec!["user4".to_string(), "user3".to_string()],
        ..team1.clone()
    };
    let dir1 = Directory {
        teams: vec![team1],
        ..Default::default()
    };
    let dir2 = Directory {
        teams: vec![team1_reordered],
        ..Default::default()
    };

    // Check no changes are detected
    assert_eq!(dir1.diff(&dir2), vec![]);
}

#[test]
fn test_diff_multiple_changes() {
    // Setup directories with team and user changes
    let team1 = Team {
        name: "team1".to_string(),
        ..Default::default()
    };
    let team1_adding_maintainer = Team {
        maintainers: vec!["user1".to_string()],
        ..team1.clone()
    };
    let team2 = Team {
        name: "team2".to_string(),
        ..Default::default()
    };
    let user1 = User {
        full_name: "user1".to_string(),
        ..Default::default()
    };
    let dir1 = Directory {
        teams: vec![team1],
        users: vec![user1],
    };
    let dir2 = Directory {
        teams: vec![team1_adding_maintainer, team2.clone()],
        ..Default::default()
    };

    // Check all the changes are detected
    assert_eq!(
        dir1.diff(&dir2),
        vec![
            DirectoryChange::TeamAdded(team2),
            DirectoryChange::TeamMaintainerAdded("team1".to_string(), "user1".to_string()),
            DirectoryChange::UserRemoved("user1".to_string())
        ]
    );
}

#[test]
fn test_diff_team_added() {
    // Setup a new team with users
    let team1 = sample_team();
    let dir1 = Directory::default();
    let dir2 = Directory {
        teams: vec![team1.clone()],
        ..Default::default()
    };

    // Check the change carries the full team and no per-user changes are emitted
    assert_eq!(dir1.diff(&dir2), vec![DirectoryChange::TeamAdded(team1)]);
}

#[test]
fn test_diff_team_maintainer_added() {
    // Setup a team gaining a maintainer
    let team1 = Team {
        name: "team1".to_string(),
        ..Default::default()
    };
    let team1_adding_maintainer = Team {
        maintainers: vec!["user1".to_string()],
        ..team1.clone()
    };
    let dir1 = Directory {
        teams: vec![team1],
        ..Default::default()
    };
    let dir2 = Directory {
        teams: vec![team1_adding_maintainer],
        ..Default::default()
    };

    // Check the change is detected
    assert_eq!(
        dir1.diff(&dir2),
        vec![DirectoryChange::TeamMaintainerAdded(
            "team1".to_string(),
            "user1".to_string()
        )]
    );
}

#[test]
fn test_diff_team_maintainer_demoted_removes_before_adding() {
    // Setup a team where a maintainer becomes a member
    let team1 = Team {
        name: "team1".to_string(),
        maintainers: vec!["user1".to_string(), "user2".to_string()],
        ..Default::default()
    };
    let team1_demoting_maintainer = Team {
        maintainers: vec!["user1".to_string()],
        members: vec!["user2".to_string()],
        ..team1.clone()
    };
    let dir1 = Directory {
        teams: vec![team1],
        ..Default::default()
    };
    let dir2 = Directory {
        teams: vec![team1_demoting_maintainer],
        ..Default::default()
    };

    // Check the removal comes first, as both changes target the same membership
    assert_eq!(
        dir1.diff(&dir2),
        vec![
            DirectoryChange::TeamMaintainerRemoved("team1".to_string(), "user2".to_string()),
            DirectoryChange::TeamMemberAdded("team1".to_string(), "user2".to_string()),
        ]
    );
}

#[test]
fn test_diff_team_maintainer_removed() {
    // Setup a team losing a maintainer
    let team1 = Team {
        name: "team1".to_string(),
        maintainers: vec!["user1".to_string()],
        ..Default::default()
    };
    let team1_removing_maintainer = Team {
        maintainers: vec![],
        ..team1.clone()
    };
    let dir1 = Directory {
        teams: vec![team1],
        ..Default::default()
    };
    let dir2 = Directory {
        teams: vec![team1_removing_maintainer],
        ..Default::default()
    };

    // Check the change is detected
    assert_eq!(
        dir1.diff(&dir2),
        vec![DirectoryChange::TeamMaintainerRemoved(
            "team1".to_string(),
            "user1".to_string()
        )]
    );
}

#[test]
fn test_diff_team_member_added() {
    // Setup a team gaining a member
    let team1 = Team {
        name: "team1".to_string(),
        ..Default::default()
    };
    let team1_adding_member = Team {
        members: vec!["user1".to_string()],
        ..team1.clone()
    };
    let dir1 = Directory {
        teams: vec![team1],
        ..Default::default()
    };
    let dir2 = Directory {
        teams: vec![team1_adding_member],
        ..Default::default()
    };

    // Check the change is detected
    assert_eq!(
        dir1.diff(&dir2),
        vec![DirectoryChange::TeamMemberAdded(
            "team1".to_string(),
            "user1".to_string()
        )]
    );
}

#[test]
fn test_diff_team_member_promoted_removes_before_adding() {
    // Setup a team where a member becomes a maintainer
    let team1 = Team {
        name: "team1".to_string(),
        maintainers: vec!["user1".to_string()],
        members: vec!["user2".to_string()],
        ..Default::default()
    };
    let team1_promoting_member = Team {
        maintainers: vec!["user1".to_string(), "user2".to_string()],
        members: vec![],
        ..team1.clone()
    };
    let dir1 = Directory {
        teams: vec![team1],
        ..Default::default()
    };
    let dir2 = Directory {
        teams: vec![team1_promoting_member],
        ..Default::default()
    };

    // Check the removal comes first, as both changes target the same membership
    assert_eq!(
        dir1.diff(&dir2),
        vec![
            DirectoryChange::TeamMemberRemoved("team1".to_string(), "user2".to_string()),
            DirectoryChange::TeamMaintainerAdded("team1".to_string(), "user2".to_string()),
        ]
    );
}

#[test]
fn test_diff_team_member_removed() {
    // Setup a team losing a member
    let team1 = Team {
        name: "team1".to_string(),
        members: vec!["user1".to_string()],
        ..Default::default()
    };
    let team1_removing_member = Team {
        members: vec![],
        ..team1.clone()
    };
    let dir1 = Directory {
        teams: vec![team1],
        ..Default::default()
    };
    let dir2 = Directory {
        teams: vec![team1_removing_member],
        ..Default::default()
    };

    // Check the change is detected
    assert_eq!(
        dir1.diff(&dir2),
        vec![DirectoryChange::TeamMemberRemoved(
            "team1".to_string(),
            "user1".to_string()
        )]
    );
}

#[test]
fn test_diff_team_removed() {
    // Setup a team that is removed
    let team1 = Team {
        name: "team1".to_string(),
        ..Default::default()
    };
    let dir1 = Directory {
        teams: vec![team1],
        ..Default::default()
    };
    let dir2 = Directory::default();

    // Check the change is detected
    assert_eq!(
        dir1.diff(&dir2),
        vec![DirectoryChange::TeamRemoved("team1".to_string())]
    );
}

#[test]
fn test_diff_user_added() {
    // Setup a new user
    let user1 = User {
        full_name: "user1".to_string(),
        ..Default::default()
    };
    let dir1 = Directory::default();
    let dir2 = Directory {
        users: vec![user1],
        ..Default::default()
    };

    // Check the change is detected
    assert_eq!(
        dir1.diff(&dir2),
        vec![DirectoryChange::UserAdded("user1".to_string())]
    );
}

#[test]
fn test_diff_user_removed() {
    // Setup a user that is removed
    let user1 = User {
        full_name: "user1".to_string(),
        ..Default::default()
    };
    let dir1 = Directory {
        users: vec![user1],
        ..Default::default()
    };
    let dir2 = Directory::default();

    // Check the change is detected
    assert_eq!(
        dir1.diff(&dir2),
        vec![DirectoryChange::UserRemoved("user1".to_string())]
    );
}

#[test]
fn test_diff_user_updated() {
    // Setup a user with updated details
    let user1 = User {
        full_name: "user1".to_string(),
        ..Default::default()
    };
    let user1_updated = User {
        user_name: Some("user1".to_string()),
        ..user1.clone()
    };
    let dir1 = Directory {
        users: vec![user1],
        ..Default::default()
    };
    let dir2 = Directory {
        users: vec![user1_updated],
        ..Default::default()
    };

    // Check the change is detected
    assert_eq!(
        dir1.diff(&dir2),
        vec![DirectoryChange::UserUpdated("user1".to_string())]
    );
}

#[tokio::test]
async fn test_get_changes_summary_diffs_base_against_head() {
    // Setup GitHub mock serving different teams in each reference
    let head = "teams: [{name: t, maintainers: [a]}, {name: n, maintainers: [a]}]";
    let mut gh = MockGH::new();
    expect_head_file(&mut gh, Ok(head));
    expect_base_file(&mut gh, PERMISSIONS_PATH, Ok(VALID_PERMISSIONS));

    // Get the changes summary
    let summary = Directory::get_changes_summary(Arc::new(gh), &sample_org(), &sample_head_source())
        .await
        .unwrap();

    // Check the team only present in head is reported as added
    assert_eq!(summary.base_ref_config_status, BaseRefConfigStatus::Valid);
    assert_eq!(summary.changes.len(), 1);
    let details = summary.changes[0].details();
    assert_eq!(details.kind, "team-added");
    assert_eq!(details.extra["team"]["name"], "n");
}

#[tokio::test]
async fn test_get_changes_summary_flags_base_config_that_cannot_be_fetched() {
    // Setup GitHub mock failing to serve the base file
    let mut gh = MockGH::new();
    expect_head_file(&mut gh, Ok(VALID_PERMISSIONS));
    expect_base_file(&mut gh, PERMISSIONS_PATH, Err("boom"));

    // Get the changes summary
    let summary = Directory::get_changes_summary(Arc::new(gh), &sample_org(), &sample_head_source())
        .await
        .unwrap();

    // Check the head configuration can still be validated
    assert_eq!(summary.base_ref_config_status, BaseRefConfigStatus::Invalid);
    assert_eq!(summary.changes.len(), 0);
}

#[tokio::test]
async fn test_get_changes_summary_flags_invalid_base_config() {
    // Setup GitHub mock serving an invalid base file
    let mut gh = MockGH::new();
    expect_head_file(&mut gh, Ok(VALID_PERMISSIONS));
    expect_base_file(&mut gh, PERMISSIONS_PATH, Ok("teams: [{name: t}]"));

    // Get the changes summary
    let summary = Directory::get_changes_summary(Arc::new(gh), &sample_org(), &sample_head_source())
        .await
        .unwrap();

    // Check the head configuration can still be validated
    assert_eq!(summary.base_ref_config_status, BaseRefConfigStatus::Invalid);
    assert_eq!(summary.changes.len(), 0);
}

#[tokio::test]
async fn test_get_changes_summary_rejects_invalid_head_config() {
    // Setup GitHub mock serving an invalid head file
    let mut gh = MockGH::new();
    expect_head_file(&mut gh, Ok("teams: [{name: t}]"));
    gh.expect_get_file_content()
        .withf(|src, _| src.ref_ == "main")
        .times(0..=1)
        .returning(|_, _| Box::pin(future::ready(Ok(VALID_PERMISSIONS.to_string()))));

    // Get the changes summary
    let result = Directory::get_changes_summary(Arc::new(gh), &sample_org(), &sample_head_source()).await;

    // Check the head configuration error is returned
    let Err(err) = result else {
        panic!("expected an error");
    };
    assert_eq!(
        format_error(&err).unwrap(),
        "\n- invalid directory configuration\n\t- team[t]: must have at least one maintainer"
    );
}

#[test]
fn test_keywords_team_added_includes_maintainers_and_members() {
    let change = DirectoryChange::TeamAdded(sample_team());
    assert_eq!(
        change.keywords(),
        vec!["team", "added", "team1", "user1", "user2"]
    );
}

#[tokio::test]
async fn test_new_from_config_aggregates_sheriff_and_cncf_errors() {
    // Setup GitHub mock serving two invalid files
    let mut gh = MockGH::new();
    expect_base_file(&mut gh, PERMISSIONS_PATH, Ok("teams: [{name: t}]"));
    expect_base_file(&mut gh, PEOPLE_PATH, Ok(r#"[{"name":""}]"#));

    // Load the directory
    let result = Directory::new_from_config(Arc::new(gh), &sample_legacy(), &sample_base_source()).await;

    // Check errors from both files are reported under a single header
    assert_eq!(
        format_error(&result.unwrap_err()).unwrap(),
        "\n- invalid directory configuration\n\t- team[t]: must have at least one maintainer\n\t- user[0]: \
         name must be provided"
    );
}

#[tokio::test]
async fn test_new_from_config_parses_cncf_people_list() {
    // Setup GitHub mock serving a people file
    let mut gh = MockGH::new();
    expect_base_file(&mut gh, PERMISSIONS_PATH, Ok(VALID_PERMISSIONS));
    expect_base_file(&mut gh, PEOPLE_PATH, Ok(r#"[{"name":"User One"}]"#));

    // Load the directory
    let directory = Directory::new_from_config(Arc::new(gh), &sample_legacy(), &sample_base_source())
        .await
        .unwrap();

    // Check the user was loaded
    assert_eq!(directory.users.len(), 1);
    assert_eq!(directory.users[0].full_name, "User One");
}

#[tokio::test]
async fn test_new_from_config_reports_cncf_fetch_error() {
    // Setup GitHub mock failing to serve the people file
    let mut gh = MockGH::new();
    expect_base_file(&mut gh, PERMISSIONS_PATH, Ok(VALID_PERMISSIONS));
    expect_base_file(&mut gh, PEOPLE_PATH, Err("boom"));

    // Load the directory
    let result = Directory::new_from_config(Arc::new(gh), &sample_legacy(), &sample_base_source()).await;

    // Check the error is reported
    assert_eq!(
        format_error(&result.unwrap_err()).unwrap(),
        "\n- invalid directory configuration\n\t- error getting cncf people file\n\t\t- boom"
    );
}

#[tokio::test]
async fn test_new_from_config_reports_sheriff_fetch_error_and_fetches_people() {
    // Setup GitHub mock failing to serve the permissions file
    let mut gh = MockGH::new();
    expect_base_file(&mut gh, PERMISSIONS_PATH, Err("boom"));
    expect_base_file(&mut gh, PEOPLE_PATH, Ok("[]"));

    // Load the directory
    let result = Directory::new_from_config(Arc::new(gh), &sample_legacy(), &sample_base_source()).await;

    // Check the error is reported
    assert_eq!(
        format_error(&result.unwrap_err()).unwrap(),
        "\n- invalid directory configuration\n\t- error getting permissions file\n\t\t- boom"
    );
}

#[tokio::test]
async fn test_new_from_config_skips_cncf_file_without_path() {
    // Setup GitHub mock serving only the permissions file
    let mut gh = MockGH::new();
    expect_base_file(&mut gh, PERMISSIONS_PATH, Ok(VALID_PERMISSIONS));
    gh.expect_get_file_content().withf(|_, path| path != PERMISSIONS_PATH).never();
    let legacy = Legacy {
        cncf_people_path: None,
        ..sample_legacy()
    };

    // Load the directory
    let directory = Directory::new_from_config(Arc::new(gh), &legacy, &sample_base_source()).await.unwrap();

    // Check no users were loaded
    assert_eq!(directory.teams.len(), 1);
    assert_eq!(directory.users, vec![]);
}

// Helpers.

/// Expect a single fetch of the file at the path provided from the base
/// reference, returning the content or error message given.
fn expect_base_file(gh: &mut MockGH, path: &'static str, result: Result<&'static str, &'static str>) {
    gh.expect_get_file_content()
        .withf(move |src, file_path| {
            src.inst_id == Some(INSTALLATION_ID)
                && src.owner == "org1"
                && src.repo == "config"
                && src.ref_ == "main"
                && file_path == path
        })
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(file_result(result))));
}

/// Expect a single fetch of the permissions file from the head reference,
/// returning the content or error message given.
fn expect_head_file(gh: &mut MockGH, result: Result<&'static str, &'static str>) {
    gh.expect_get_file_content()
        .withf(|src, path| {
            src.inst_id == Some(INSTALLATION_ID)
                && src.owner == "fork-owner"
                && src.repo == "fork-repo"
                && src.ref_ == "head-branch"
                && path == PERMISSIONS_PATH
        })
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(file_result(result))));
}

/// Convert the file result provided into the one returned by the GH trait.
fn file_result(result: Result<&str, &str>) -> anyhow::Result<String> {
    result.map(ToString::to_string).map_err(|err| format_err!("{err}"))
}

/// Return the source of the base reference of the sample organization.
fn sample_base_source() -> Source {
    Source::from(&sample_org())
}

/// Return the source of a pull request's head in a fork.
fn sample_head_source() -> Source {
    Source {
        inst_id: Some(INSTALLATION_ID),
        owner: "fork-owner".to_string(),
        repo: "fork-repo".to_string(),
        ref_: "head-branch".to_string(),
    }
}

/// Return a legacy configuration using both the permissions and people files.
fn sample_legacy() -> Legacy {
    Legacy {
        enabled: true,
        sheriff_permissions_path: PERMISSIONS_PATH.to_string(),
        cncf_people_path: Some(PEOPLE_PATH.to_string()),
    }
}

/// Return an organization with a configuration without people file.
fn sample_org() -> Organization {
    Organization {
        name: "org1".to_string(),
        installation_id: INSTALLATION_ID,
        repository: "config".to_string(),
        branch: "main".to_string(),
        legacy: Legacy {
            cncf_people_path: None,
            ..sample_legacy()
        },
    }
}

/// Return a team with a maintainer and a member.
fn sample_team() -> Team {
    Team {
        name: "team1".to_string(),
        maintainers: vec!["user1".to_string()],
        members: vec!["user2".to_string()],
        ..Default::default()
    }
}
