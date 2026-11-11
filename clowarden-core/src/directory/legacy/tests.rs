use std::{future, sync::Arc};

use anyhow::{Error, Result};

use crate::{
    directory::Team,
    github::{MockGH, Source},
    multierror::MultiError,
};

use super::{cncf, sheriff};

const PEOPLE_PATH: &str = "people.json";
const PERMISSIONS_PATH: &str = "config/permissions.yaml";

#[tokio::test]
async fn test_cncf_cfg_get_rejects_invalid_json() {
    // Setup GitHub mock serving an invalid people file
    let mut gh = MockGH::new();
    gh.expect_get_file_content()
        .withf(|src, path| src.ref_ == "main" && path == PEOPLE_PATH)
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok("{".to_string()))));

    // Get the configuration
    let err = cncf::Cfg::get(Arc::new(gh), &sample_source(), Some(PEOPLE_PATH)).await.unwrap_err();

    // Check the parsing error is reported
    assert_eq!(err.to_string(), "error parsing cncf people file");
}

#[tokio::test]
async fn test_sheriff_cfg_get_formation_dedupes_and_sorts_users() {
    // Setup a team that overlaps with two subteams
    let content = "teams: [
        {name: c, maintainers: [b], members: [y], formation: [s1, s2]},
        {name: s1, maintainers: [a, b], members: [x, y]},
        {name: s2, maintainers: [a], members: [x]},
    ]";

    // Get the configuration
    let cfg = get_sheriff_cfg(content).await.unwrap();

    // Check the users of the team are sorted and unique
    assert_eq!(cfg.teams[0].maintainers, Some(users(&["a", "b"])));
    assert_eq!(cfg.teams[0].members, Some(users(&["x", "y"])));
}

#[tokio::test]
async fn test_sheriff_cfg_get_formation_handles_subteams_missing_fields() {
    // Setup teams using a subteam without members
    let content = "teams: [
        {name: c, formation: [s2, s1]},
        {name: d, maintainers: [a], members: [x], formation: [s2]},
        {name: s1, maintainers: [a], members: [b]},
        {name: s2, maintainers: [a]},
    ]";

    // Get the configuration
    let cfg = get_sheriff_cfg(content).await.unwrap();

    // Check members from the other sources were preserved
    assert_eq!(cfg.teams[0].members, Some(users(&["b"])));
    assert_eq!(cfg.teams[1].members, Some(users(&["x"])));
}

#[tokio::test]
async fn test_sheriff_cfg_get_formation_inherits_subteam_users() {
    // Setup a team defined only through a subteam
    let content = "teams: [
        {name: c, formation: [s1]},
        {name: s1, maintainers: [a], members: [b]},
    ]";

    // Get the configuration
    let cfg = get_sheriff_cfg(content).await.unwrap();

    // Check the team passed validation with the users of the subteam
    assert_eq!(cfg.teams[0].maintainers, Some(users(&["a"])));
    assert_eq!(cfg.teams[0].members, Some(users(&["b"])));
}

#[tokio::test]
async fn test_sheriff_cfg_get_formation_is_not_recursive() {
    // Setup a subteam that uses formation too, listed before the team using it
    let content = "teams: [
        {name: s0, maintainers: [z]},
        {name: s1, maintainers: [a], formation: [s0]},
        {name: c, formation: [s1]},
    ]";

    // Get the configuration
    let cfg = get_sheriff_cfg(content).await.unwrap();

    // Check only the users defined in the subteam itself are inherited
    assert_eq!(cfg.teams[1].maintainers, Some(users(&["a", "z"])));
    assert_eq!(cfg.teams[2].maintainers, Some(users(&["a"])));
}

#[tokio::test]
async fn test_sheriff_cfg_get_rejects_duplicate_team() {
    // Get a configuration with a team defined twice
    let content = "teams: [{name: t, maintainers: [a]}, {name: t}]";
    let err = get_sheriff_cfg(content).await.unwrap_err();

    // Check the second entry is not validated any further
    assert_eq!(errors(&err), vec!["team[t]: duplicate config for team t"]);
}

#[tokio::test]
async fn test_sheriff_cfg_get_rejects_empty_team_name() {
    // Get a configuration with a team without name
    let content = "teams: [{name: '', maintainers: [a]}]";
    let err = get_sheriff_cfg(content).await.unwrap_err();

    // Check the team is identified by its position
    assert_eq!(
        errors(&err),
        vec![
            "team[0]: name must be provided",
            "team[0]: name must be lowercase alphanumeric with dashes (team slug)",
        ]
    );
}

#[tokio::test]
async fn test_sheriff_cfg_get_rejects_invalid_team_name() {
    // Get a configuration with a team name that is not a valid slug
    let content = "teams: [{name: Team_A, maintainers: [a]}]";
    let err = get_sheriff_cfg(content).await.unwrap_err();

    // Check the name is rejected
    assert_eq!(
        errors(&err),
        vec!["team[Team_A]: name must be lowercase alphanumeric with dashes (team slug)"]
    );
}

#[tokio::test]
async fn test_sheriff_cfg_get_rejects_team_without_maintainers() {
    // Get a configuration with omitted and empty maintainers
    let content = "teams: [{name: t1}, {name: t2, maintainers: []}]";
    let err = get_sheriff_cfg(content).await.unwrap_err();

    // Check both teams are rejected
    assert_eq!(
        errors(&err),
        vec![
            "team[t1]: must have at least one maintainer",
            "team[t2]: must have at least one maintainer",
        ]
    );
}

#[tokio::test]
async fn test_sheriff_cfg_get_rejects_unknown_formation_team() {
    // Get a configuration referencing a team that is not defined
    let content = "teams: [{name: c, maintainers: [a], formation: [typo-team]}]";
    let err = get_sheriff_cfg(content).await.unwrap_err();

    // Check the unknown team is reported
    assert_eq!(errors(&err), vec!["team[c]: unknown team typo-team in formation"]);
}

#[tokio::test]
async fn test_sheriff_cfg_get_rejects_user_with_both_roles() {
    // Get a configuration with a user that is maintainer and member
    let content = "teams: [{name: t, maintainers: [a], members: [a]}]";
    let err = get_sheriff_cfg(content).await.unwrap_err();

    // Check the user is reported
    assert_eq!(
        errors(&err),
        vec!["team[t]: a must be either a maintainer or a member, but not both"]
    );
}

#[tokio::test]
async fn test_sheriff_cfg_get_reports_all_errors() {
    // Get a configuration with several invalid teams
    let content = "teams: [
        {name: Team_A, maintainers: [a]},
        {name: t1},
        {name: t2, maintainers: [a], members: [a]},
        {name: t3, formation: [typo-team]},
    ]";
    let err = get_sheriff_cfg(content).await.unwrap_err();

    // Check every error is reported
    assert_eq!(
        errors(&err),
        vec![
            "team[Team_A]: name must be lowercase alphanumeric with dashes (team slug)",
            "team[t1]: must have at least one maintainer",
            "team[t2]: a must be either a maintainer or a member, but not both",
            "team[t3]: unknown team typo-team in formation",
            "team[t3]: must have at least one maintainer",
        ]
    );
}

#[tokio::test]
async fn test_sheriff_cfg_get_returns_valid_config() {
    // Get a valid configuration
    let content = "teams: [{name: team-1, maintainers: [a], members: [b]}]";
    let cfg = get_sheriff_cfg(content).await.unwrap();

    // Check the team returned
    assert_eq!(
        cfg.teams,
        vec![sheriff::Team {
            name: "team-1".to_string(),
            maintainers: Some(users(&["a"])),
            members: Some(users(&["b"])),
            formation: None,
        }]
    );
}

#[test]
fn test_sheriff_team_from_directory_team_keeps_name_and_users() {
    // Setup directory team
    let team = Team {
        name: "team-1".to_string(),
        maintainers: users(&["a"]),
        members: users(&["b"]),
        ..Default::default()
    };

    // Convert the team
    let team = sheriff::Team::from(team);

    // Check the name and users were kept
    assert_eq!(team.name, "team-1");
    assert_eq!(team.maintainers, Some(users(&["a"])));
    assert_eq!(team.members, Some(users(&["b"])));
}

// Helpers.

/// Return the messages of the errors aggregated in the error provided.
fn errors(err: &Error) -> Vec<String> {
    let merr = err.downcast_ref::<MultiError>().expect("error to be a MultiError");
    merr.errors().iter().map(ToString::to_string).collect()
}

/// Get the sheriff configuration from a permissions file with the content
/// provided.
async fn get_sheriff_cfg(content: &'static str) -> Result<sheriff::Cfg> {
    let mut gh = MockGH::new();
    gh.expect_get_file_content()
        .withf(|src, path| src.ref_ == "main" && path == PERMISSIONS_PATH)
        .times(1)
        .returning(move |_, _| Box::pin(future::ready(Ok(content.to_string()))));
    sheriff::Cfg::get(Arc::new(gh), &sample_source(), PERMISSIONS_PATH).await
}

/// Return the source used to get the files in these tests.
fn sample_source() -> Source {
    Source {
        inst_id: None,
        owner: "org1".to_string(),
        repo: "config".to_string(),
        ref_: "main".to_string(),
    }
}

/// Build a list with the users provided.
fn users(names: &[&str]) -> Vec<String> {
    names.iter().map(ToString::to_string).collect()
}
