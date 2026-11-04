use std::{collections::BTreeMap, sync::Arc};

use crate::{
    github::MockGH,
    multierror::MultiError,
    services::github::{
        state::{Repository, Role, Visibility},
        testing::{BASE_REF, CONFIG_PATH, expect_config_file, multierror_messages, sample_src},
    },
};

use super::Cfg;

#[tokio::test]
async fn test_cfg_get_parses_repositories() {
    // Setup the configuration file, using the documented collaborators key
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
repositories:
  - name: repo1
    external_collaborators:
      user1: write
    teams:
      team1: maintain
    visibility: private
",
    );

    // Get the configuration
    let cfg = Cfg::get(Arc::new(gh), &sample_src(), CONFIG_PATH).await.unwrap();

    // Check the repository was parsed as expected
    assert_eq!(
        cfg.repositories,
        vec![Repository {
            name: "repo1".to_string(),
            collaborators: Some(BTreeMap::from([("user1".to_string(), Role::Write)])),
            teams: Some(BTreeMap::from([("team1".to_string(), Role::Maintain)])),
            visibility: Some(Visibility::Private),
        }]
    );
}

#[tokio::test]
async fn test_cfg_get_rejects_invalid_repositories() {
    // Setup a configuration file with an invalid repository
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
repositories:
  - name: repo1
    teams:
      Bad_Team: read
",
    );

    // Get the configuration
    let err = Cfg::get(Arc::new(gh), &sample_src(), CONFIG_PATH).await.unwrap_err();

    // Check validation runs on the configuration parsed
    assert_eq!(
        multierror_messages(&err),
        vec!["repo[repo1]: team[Bad_Team] name must be lowercase alphanumeric with dashes (team slug)"]
    );
}

#[tokio::test]
async fn test_cfg_get_rejects_invalid_role() {
    // Setup a configuration file using a role that does not exist
    let mut gh = MockGH::new();
    expect_config_file(
        &mut gh,
        BASE_REF,
        1,
        "
repositories:
  - name: repo1
    external_collaborators:
      user1: owner
",
    );

    // Get the configuration
    let err = Cfg::get(Arc::new(gh), &sample_src(), CONFIG_PATH).await.unwrap_err();

    // Check the file was reported as unparseable
    assert_eq!(err.to_string(), "error parsing permissions file");
}

#[test]
fn test_cfg_validate_accepts_valid_repositories() {
    // Setup some repositories using valid names and team slugs
    let cfg = Cfg {
        repositories: vec![
            sample_repository("repo1", &["team-1"]),
            sample_repository("repo2", &["team-1", "team2"]),
        ],
    };

    // Check the configuration is valid
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_cfg_validate_rejects_duplicate_repository() {
    // Setup a repository that is defined twice
    let cfg = Cfg {
        repositories: vec![sample_repository("repo1", &[]), sample_repository("repo1", &[])],
    };

    // Validate the configuration
    let err = cfg.validate().unwrap_err();

    // Check the duplicate was reported once
    assert_eq!(
        multierror_messages(&err),
        vec!["repo[repo1]: duplicate config for repo repo1"]
    );
}

#[test]
fn test_cfg_validate_accepts_names_github_allows() {
    // Setup repositories using all the characters allowed
    let cfg = Cfg {
        repositories: vec![
            sample_repository(".github", &[]),
            sample_repository("My_Repo-1.x", &[]),
        ],
    };

    // Check the configuration is valid
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_cfg_validate_rejects_invalid_repository_name() {
    // Setup repositories whose names could alter how PR comments render
    let cfg = Cfg {
        repositories: vec![
            sample_repository("repo1\n\n<!--", &[]),
            sample_repository("repo 2", &[]),
            sample_repository("**repo3**", &[]),
        ],
    };

    // Validate the configuration
    let err = cfg.validate().unwrap_err();

    // Check the names were reported without being repeated in the messages
    assert_eq!(
        multierror_messages(&err),
        vec![
            "repo[0]: name must contain only letters, numbers, hyphens, underscores or periods",
            "repo[1]: name must contain only letters, numbers, hyphens, underscores or periods",
            "repo[2]: name must contain only letters, numbers, hyphens, underscores or periods",
        ]
    );
}

#[test]
fn test_cfg_validate_rejects_invalid_team_slug() {
    // Setup a repository that uses a team name that is not a slug
    let cfg = Cfg {
        repositories: vec![sample_repository("repo1", &["Bad_Team"])],
    };

    // Validate the configuration
    let err = cfg.validate().unwrap_err();

    // Check the team was reported
    assert_eq!(
        multierror_messages(&err),
        vec!["repo[repo1]: team[Bad_Team] name must be lowercase alphanumeric with dashes (team slug)"]
    );
}

#[test]
fn test_cfg_validate_rejects_repository_without_name() {
    // Setup a repository without name that is not the first one on the list
    let cfg = Cfg {
        repositories: vec![sample_repository("repo1", &[]), sample_repository("", &[])],
    };

    // Validate the configuration
    let err = cfg.validate().unwrap_err();

    // Check the repository was reported using its index
    assert_eq!(multierror_messages(&err), vec!["repo[1]: name must be provided"]);
}

#[test]
fn test_cfg_validate_reports_all_errors() {
    // Setup some repositories with different problems
    let cfg = Cfg {
        repositories: vec![
            sample_repository("", &[]),
            sample_repository("repo1", &["Bad_Team"]),
            sample_repository("repo1", &[]),
        ],
    };

    // Validate the configuration
    let err = cfg.validate().unwrap_err();

    // Check all problems were aggregated under the service context
    assert_eq!(
        err.downcast_ref::<MultiError>().unwrap().context.as_deref(),
        Some("invalid github service configuration")
    );
    assert_eq!(
        multierror_messages(&err),
        vec![
            "repo[0]: name must be provided",
            "repo[repo1]: duplicate config for repo repo1",
            "repo[repo1]: team[Bad_Team] name must be lowercase alphanumeric with dashes (team slug)",
        ]
    );
}

// Helpers.

/// Build a repository with the name and teams provided.
fn sample_repository(name: &str, teams: &[&str]) -> Repository {
    Repository {
        name: name.to_string(),
        teams: Some(teams.iter().map(|team_name| ((*team_name).to_string(), Role::Read)).collect()),
        ..Default::default()
    }
}
