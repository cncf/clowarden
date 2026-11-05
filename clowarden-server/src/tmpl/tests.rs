use std::{collections::BTreeMap, env, fs};

use anyhow::anyhow;
use clowarden_core::{
    directory::{DirectoryChange, Team},
    multierror::MultiError,
    services::{
        BaseRefConfigStatus, ChangeApplied, DynChange,
        github::state::{Repository, RepositoryChange, Role, Visibility},
    },
};
use time::OffsetDateTime;

use super::*;

const BASE_REF_WARNING: &str = "The configuration in the base reference could not be read or is not valid, \
                                so I cannot check what has changed. Please review changes manually.";
const GITHUB: ServiceName = "github";
const TESTDATA_PATH: &str = "src/testdata";

#[test]
fn test_reconciliation_completed_success_no_changes() {
    // Setup a reconciliation that had nothing to do in the service
    let changes_applied = HashMap::from([(GITHUB, vec![])]);
    let errors = HashMap::new();

    // Render the comment
    let tmpl = ReconciliationCompleted::new(&changes_applied, &errors);
    let output = tmpl.render().unwrap();

    // Check the comment reports no changes and has no service section
    check_golden_file("reconciliation-completed-success-no-changes", &output);
}

#[test]
fn test_reconciliation_completed_success_with_changes() {
    // Setup the changes applied
    let changes_applied = HashMap::from([(
        GITHUB,
        vec![
            change_applied(
                Box::new(RepositoryChange::RepositoryAdded(create_test_repository(
                    "test-repo",
                ))),
                None,
            ),
            change_applied(
                Box::new(RepositoryChange::TeamAdded(
                    "test-repo".to_string(),
                    "engineering".to_string(),
                    Role::Write,
                )),
                None,
            ),
            change_applied(
                Box::new(RepositoryChange::CollaboratorAdded(
                    "test-repo".to_string(),
                    "alice".to_string(),
                    Role::Admin,
                )),
                None,
            ),
        ],
    )]);
    let errors = HashMap::new();

    // Render the comment
    let tmpl = ReconciliationCompleted::new(&changes_applied, &errors);
    let output = tmpl.render().unwrap();

    // Check the comment lists all changes
    check_golden_file("reconciliation-completed-success-with-changes", &output);
}

#[test]
fn test_reconciliation_completed_success_with_teams_and_collaborators() {
    // Setup a new repository with teams and collaborators
    let mut repo = create_test_repository("complex-repo");
    repo.teams = Some(BTreeMap::from([
        ("backend".to_string(), Role::Admin),
        ("frontend".to_string(), Role::Write),
    ]));
    repo.collaborators = Some(BTreeMap::from([
        ("jane".to_string(), Role::Read),
        ("john".to_string(), Role::Maintain),
    ]));
    let changes_applied = HashMap::from([(
        GITHUB,
        vec![change_applied(
            Box::new(RepositoryChange::RepositoryAdded(repo)),
            None,
        )],
    )]);
    let errors = HashMap::new();

    // Render the comment
    let tmpl = ReconciliationCompleted::new(&changes_applied, &errors);
    let output = tmpl.render().unwrap();

    // Check the comment lists the repository details
    check_golden_file(
        "reconciliation-completed-success-with-teams-and-collaborators",
        &output,
    );
}

#[test]
fn test_reconciliation_completed_with_change_errors() {
    // Setup a change that failed
    let changes_applied = HashMap::from([(
        GITHUB,
        vec![change_applied(
            Box::new(RepositoryChange::TeamAdded(
                "test-repo".to_string(),
                "engineering".to_string(),
                Role::Write,
            )),
            Some("Team not found in directory"),
        )],
    )]);
    let errors = HashMap::new();

    // Render the comment
    let tmpl = ReconciliationCompleted::new(&changes_applied, &errors);
    let output = tmpl.render().unwrap();

    // Check the comment reports the failure
    check_golden_file("reconciliation-completed-with-change-errors", &output);
}

#[test]
fn test_reconciliation_completed_with_failure_after_successful_change() {
    // Setup a mix of changes where the first ones were applied successfully
    let change = |team: &str| {
        Box::new(RepositoryChange::TeamRemoved(
            "test-repo".to_string(),
            team.to_string(),
        )) as DynChange
    };
    let changes_applied = HashMap::from([(
        GITHUB,
        vec![
            change_applied(change("team-1"), None),
            change_applied(change("team-2"), Some("first failure")),
            change_applied(change("team-3"), None),
            change_applied(change("team-4"), Some("second failure")),
        ],
    )]);
    let errors = HashMap::new();

    // Render the comment
    let tmpl = ReconciliationCompleted::new(&changes_applied, &errors);
    let output = tmpl.render().unwrap();

    // Check the comment reports the failures and does not claim success
    assert!(output.contains("Some errors were found during the reconciliation"));
    assert!(output.contains("first failure"));
    assert!(output.contains("second failure"));
    assert!(!output.contains("all changes have been applied"));

    // Check the comment lists the changes that were applied
    check_golden_file(
        "reconciliation-completed-with-failure-after-successful-change",
        &output,
    );
}

#[test]
fn test_reconciliation_completed_with_service_errors() {
    // Setup a service that failed as a whole
    let changes_applied = HashMap::new();
    let errors = HashMap::from([(GITHUB, anyhow!("Failed to connect to GitHub API"))]);

    // Render the comment
    let tmpl = ReconciliationCompleted::new(&changes_applied, &errors);
    let output = tmpl.render().unwrap();

    // Check the comment reports the failure
    check_golden_file("reconciliation-completed-with-service-errors", &output);
}

#[test]
fn test_validation_failed_directory_error_tree() {
    // Setup the error tree an invalid directory configuration produces
    let mut sheriff_errors = MultiError::new(None);
    sheriff_errors.push(anyhow!(
        "team[core]: <bob> & \"co\" must be either a maintainer or a member, but not both"
    ));
    let mut directory_errors = MultiError::new(Some("invalid directory configuration".to_string()));
    directory_errors.push(sheriff_errors.into());
    directory_errors.push(anyhow!("missing field `name`").context("error parsing cncf people file"));
    let err = validation_error(vec![
        Error::from(directory_errors).context("invalid directory configuration"),
    ]);

    // Render the comment
    let tmpl = ValidationFailed::new(&err);
    let output = tmpl.render().unwrap();

    // Check the comment nests the errors, printing their context just once
    check_golden_file("validation-failed-directory-error-tree", &output);
}

#[test]
fn test_validation_failed_multi_error() {
    // Setup the error tree an invalid service configuration produces
    let mut service_errors = MultiError::new(Some("Configuration validation failed".to_string()));
    service_errors.push(anyhow!("Missing required field: teams"));
    service_errors.push(anyhow!("Invalid repository name: contains spaces"));
    let err = validation_error(vec![service_errors.into()]);

    // Render the comment
    let tmpl = ValidationFailed::new(&err);
    let output = tmpl.render().unwrap();

    // Check the comment nests the errors under their context
    check_golden_file("validation-failed-multi-error", &output);
}

#[test]
fn test_validation_failed_nested_error() {
    // Setup an error with a chain of causes
    let err = validation_error(vec![
        anyhow!("Failed to parse configuration")
            .context("Unable to read configuration file")
            .context("Configuration validation failed"),
    ]);

    // Render the comment
    let tmpl = ValidationFailed::new(&err);
    let output = tmpl.render().unwrap();

    // Check the comment lists the causes
    check_golden_file("validation-failed-nested-error", &output);
}

#[test]
fn test_validation_failed_simple_error() {
    // Setup a single error
    let err = validation_error(vec![anyhow!("Invalid configuration format")]);

    // Render the comment
    let tmpl = ValidationFailed::new(&err);
    let output = tmpl.render().unwrap();

    // Check the comment lists the error
    check_golden_file("validation-failed-simple-error", &output);
}

#[test]
fn test_validation_succeeded_change_variants() {
    // Setup changes using distinct names so that any swap is visible
    let directory_changes = changes_summary(
        vec![
            Box::new(DirectoryChange::TeamMaintainerAdded(
                "team-1".to_string(),
                "user-1".to_string(),
            )),
            Box::new(DirectoryChange::TeamMaintainerRemoved(
                "team-2".to_string(),
                "user-2".to_string(),
            )),
            Box::new(DirectoryChange::TeamMemberRemoved(
                "team-3".to_string(),
                "user-3".to_string(),
            )),
            Box::new(DirectoryChange::UserRemoved("User Four".to_string())),
            Box::new(DirectoryChange::UserUpdated("User Five".to_string())),
        ],
        BaseRefConfigStatus::Valid,
    );
    let services_changes = HashMap::from([(
        GITHUB,
        changes_summary(
            vec![
                Box::new(RepositoryChange::TeamRoleUpdated(
                    "repo-1".to_string(),
                    "team-6".to_string(),
                    Role::Maintain,
                )),
                Box::new(RepositoryChange::CollaboratorRemoved(
                    "repo-2".to_string(),
                    "user-7".to_string(),
                )),
                Box::new(RepositoryChange::CollaboratorRoleUpdated(
                    "repo-3".to_string(),
                    "user-8".to_string(),
                    Role::Triage,
                )),
            ],
            BaseRefConfigStatus::Valid,
        ),
    )]);

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check each change says who is affected, where, and how
    for line in [
        "- **user-1** is now a maintainer of team **team-1**",
        "- **user-2** is no longer a maintainer of team **team-2**",
        "- **user-3** is no longer a member of team **team-3**",
        "- user **User Four** has been *removed*",
        "- user **User Five** details have been *updated*",
        "- team **team-6** role in repository **repo-1** has been *updated* to **maintain**",
        "- user **user-7** is no longer a collaborator of repository **repo-2**",
        "- user **user-8** role in repository **repo-3** has been updated to **triage**",
    ] {
        assert!(output.contains(line), "line not found: {line}\n{output}");
    }
}

#[test]
fn test_validation_succeeded_directory_and_service_changes() {
    // Setup changes in the directory and in the service
    let directory_changes = changes_summary(
        vec![Box::new(DirectoryChange::UserAdded("Bob Jones".to_string()))],
        BaseRefConfigStatus::Valid,
    );
    let services_changes = HashMap::from([(
        GITHUB,
        changes_summary(
            vec![Box::new(RepositoryChange::TeamRemoved(
                "old-repo".to_string(),
                "legacy-team".to_string(),
            ))],
            BaseRefConfigStatus::Valid,
        ),
    )]);

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check the comment has a section for each of them
    check_golden_file("validation-succeeded-directory-and-service-changes", &output);
}

#[test]
fn test_validation_succeeded_directory_changes() {
    // Setup directory changes that do not affect the service
    let directory_changes = changes_summary(
        vec![
            Box::new(DirectoryChange::UserAdded("Alice Smith".to_string())),
            Box::new(DirectoryChange::TeamAdded(create_test_team("engineering"))),
            Box::new(DirectoryChange::TeamMemberAdded(
                "engineering".to_string(),
                "alice".to_string(),
            )),
        ],
        BaseRefConfigStatus::Valid,
    );
    let services_changes = HashMap::from([(GITHUB, changes_summary(vec![], BaseRefConfigStatus::Valid))]);

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check the comment has a directory section and no empty service section
    check_golden_file("validation-succeeded-directory-changes", &output);
}

#[test]
fn test_validation_succeeded_directory_changes_and_invalid_base_ref_service() {
    // Setup directory changes next to a service that cannot load the base config
    let directory_changes = changes_summary(
        vec![Box::new(DirectoryChange::TeamRemoved("old-team".to_string()))],
        BaseRefConfigStatus::Valid,
    );
    let services_changes = HashMap::from([(GITHUB, changes_summary(vec![], BaseRefConfigStatus::Invalid))]);

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check the comment lists the changes and warns about the partial list
    assert!(output.contains(BASE_REF_WARNING));
    check_golden_file(
        "validation-succeeded-directory-changes-and-invalid-base-ref-service",
        &output,
    );
}

#[test]
fn test_validation_succeeded_invalid_base_ref_directory() {
    // Setup a base directory config that cannot be loaded, with no services
    let directory_changes = changes_summary(vec![], BaseRefConfigStatus::Invalid);
    let services_changes = HashMap::new();

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check the comment warns about it in the directory section
    assert!(output.contains(BASE_REF_WARNING));
    check_golden_file("validation-succeeded-invalid-base-ref-directory", &output);
}

#[test]
fn test_validation_succeeded_invalid_base_ref_directory_and_service() {
    // Setup a base directory config that cannot be loaded, which fails the service too
    let directory_changes = changes_summary(vec![], BaseRefConfigStatus::Invalid);
    let services_changes = HashMap::from([(GITHUB, changes_summary(vec![], BaseRefConfigStatus::Invalid))]);

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check the comment warns about it in both sections
    assert_eq!(output.matches(BASE_REF_WARNING).count(), 2);
    check_golden_file(
        "validation-succeeded-invalid-base-ref-directory-and-service",
        &output,
    );
}

#[test]
fn test_validation_succeeded_invalid_base_ref_service() {
    // Setup a service that cannot load the base config
    let directory_changes = changes_summary(vec![], BaseRefConfigStatus::Valid);
    let services_changes = HashMap::from([(GITHUB, changes_summary(vec![], BaseRefConfigStatus::Invalid))]);

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check the comment warns about it in the service section
    assert!(output.contains(BASE_REF_WARNING));
    check_golden_file("validation-succeeded-invalid-base-ref-service", &output);
}

#[test]
fn test_validation_succeeded_invalid_base_ref_service_with_warnings() {
    // Setup a service that cannot load the base config and found user problems
    let directory_changes = changes_summary(vec![], BaseRefConfigStatus::Valid);
    let mut service_changes = changes_summary(vec![], BaseRefConfigStatus::Invalid);
    service_changes.warnings = vec![
        "user[alice]: invalid username, should be Alice".to_string(),
        "user[bob]: error validating username: not found".to_string(),
    ];
    let services_changes = HashMap::from([(GITHUB, service_changes)]);

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check the comment lists the problems found
    check_golden_file(
        "validation-succeeded-invalid-base-ref-service-with-warnings",
        &output,
    );
}

#[test]
fn test_validation_succeeded_no_changes() {
    // Setup summaries with no changes for the directory and the service
    let directory_changes = changes_summary(vec![], BaseRefConfigStatus::Valid);
    let services_changes = HashMap::from([(GITHUB, changes_summary(vec![], BaseRefConfigStatus::Valid))]);

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check the comment reports no changes and has no sections
    check_golden_file("validation-succeeded-no-changes", &output);
}

#[test]
fn test_validation_succeeded_repository_added_with_no_collaborators_left() {
    // Setup a new repository whose collaborators were all organization admins
    let mut repo = create_test_repository("new-repo");
    repo.collaborators = Some(BTreeMap::new());
    let directory_changes = changes_summary(vec![], BaseRefConfigStatus::Valid);
    let services_changes = HashMap::from([(
        GITHUB,
        changes_summary(
            vec![Box::new(RepositoryChange::RepositoryAdded(repo))],
            BaseRefConfigStatus::Valid,
        ),
    )]);

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check the comment lists the repository without an empty collaborators list
    assert!(output.contains("- repository **new-repo** has been *added* (visibility: **public**)"));
    assert!(!output.contains("Collaborators"));
}

#[test]
fn test_validation_succeeded_service_changes() {
    // Setup service changes
    let directory_changes = changes_summary(vec![], BaseRefConfigStatus::Valid);
    let services_changes = HashMap::from([(
        GITHUB,
        changes_summary(
            vec![
                Box::new(RepositoryChange::RepositoryAdded(create_test_repository(
                    "new-repo",
                ))),
                Box::new(RepositoryChange::VisibilityUpdated(
                    "existing-repo".to_string(),
                    Visibility::Private,
                )),
            ],
            BaseRefConfigStatus::Valid,
        ),
    )]);

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check the comment has a service section and no empty directory section
    check_golden_file("validation-succeeded-service-changes", &output);
}

#[test]
fn test_validation_succeeded_team_with_members() {
    // Setup a new team with maintainers and members
    let mut team = create_test_team("devops");
    team.maintainers = vec!["alice".to_string(), "bob".to_string()];
    team.members = vec!["charlie".to_string(), "dave".to_string(), "eve".to_string()];
    let directory_changes = changes_summary(
        vec![Box::new(DirectoryChange::TeamAdded(team))],
        BaseRefConfigStatus::Valid,
    );
    let services_changes = HashMap::from([(GITHUB, changes_summary(vec![], BaseRefConfigStatus::Valid))]);

    // Render the comment
    let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
    let output = tmpl.render().unwrap();

    // Check the comment lists the team details
    check_golden_file("validation-succeeded-team-with-members", &output);
}

// Helpers.

/// Create a change applied, successfully unless an error is provided.
fn change_applied(change: DynChange, error: Option<&str>) -> ChangeApplied {
    ChangeApplied {
        change,
        error: error.map(ToString::to_string),
        applied_at: OffsetDateTime::UNIX_EPOCH,
    }
}

/// Create a changes summary.
fn changes_summary(changes: Vec<DynChange>, base_ref_config_status: BaseRefConfigStatus) -> ChangesSummary {
    ChangesSummary {
        changes,
        base_ref_config_status,
        warnings: vec![],
    }
}

/// Check the output matches the golden file provided, or update the golden
/// file instead when REGENERATE_GOLDEN_FILES is set.
fn check_golden_file(name: &str, actual: &str) {
    if env::var("REGENERATE_GOLDEN_FILES").is_ok() {
        write_golden_file(name, actual);
    } else {
        let expected = read_golden_file(name);
        assert_eq!(actual, expected, "output does not match golden file ({name})");
    }
}

/// Create a public repository with no teams or collaborators.
fn create_test_repository(name: &str) -> Repository {
    Repository {
        name: name.to_string(),

        collaborators: None,
        teams: None,
        visibility: Some(Visibility::Public),
    }
}

/// Create a team with no maintainers or members.
fn create_test_team(name: &str) -> Team {
    Team {
        name: name.to_string(),

        display_name: None,
        maintainers: vec![],
        members: vec![],
        annotations: HashMap::new(),
    }
}

/// Return the path of the golden file provided.
fn golden_file_path(name: &str) -> String {
    format!("{TESTDATA_PATH}/templates/{name}.golden")
}

/// Read the golden file provided.
fn read_golden_file(name: &str) -> String {
    let path = golden_file_path(name);
    fs::read_to_string(&path).unwrap_or_else(|_| panic!("error reading golden file: {path}"))
}

/// Create the top level error of a validate job, which has no context and
/// aggregates the errors found.
fn validation_error(errors: Vec<Error>) -> Error {
    let mut merr = MultiError::new(None);
    for err in errors {
        merr.push(err);
    }
    merr.into()
}

/// Write the golden file provided.
fn write_golden_file(name: &str, content: &str) {
    let path = golden_file_path(name);
    fs::write(&path, content).expect("write golden file should succeed");
}
