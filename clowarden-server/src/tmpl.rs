//! This module defines the templates used to render the comments that
//! CLOWarden will post to GitHub.

use std::collections::HashMap;

use anyhow::Error;
use askama::Template;

use clowarden_core::services::{ChangesApplied, ChangesSummary, ServiceName};

/// Template for the reconciliation completed comment.
#[derive(Template)]
#[template(path = "reconciliation-completed.md")]
pub(crate) struct ReconciliationCompleted<'a> {
    services: Vec<ServiceName>,
    changes_applied: &'a HashMap<ServiceName, ChangesApplied>,
    some_changes_applied: bool,
    errors: &'a HashMap<ServiceName, Error>,
    errors_found: bool,
}

impl<'a> ReconciliationCompleted<'a> {
    pub(crate) fn new(
        changes_applied: &'a HashMap<ServiceName, ChangesApplied>,
        errors: &'a HashMap<ServiceName, Error>,
    ) -> Self {
        let services = changes_applied.keys().chain(errors.keys()).copied().collect();
        let some_changes_applied = (|| {
            for service_changes_applied in changes_applied.values() {
                if !service_changes_applied.is_empty() {
                    return true;
                }
            }
            false
        })();
        let errors_found = (|| {
            if !errors.is_empty() {
                return true;
            }
            for service_changes_applied in changes_applied.values() {
                for change_applied in service_changes_applied {
                    if change_applied.error.is_some() {
                        return true;
                    }
                }
            }
            false
        })();
        Self {
            services,
            changes_applied,
            some_changes_applied,
            errors,
            errors_found,
        }
    }
}

/// Template for the validation failed comment.
#[derive(Template)]
#[template(path = "validation-failed.md")]
pub(crate) struct ValidationFailed<'a> {
    err: &'a Error,
}

impl<'a> ValidationFailed<'a> {
    pub(crate) fn new(err: &'a Error) -> Self {
        Self { err }
    }
}

/// Template for the validation succeeded comment.
#[derive(Template)]
#[template(path = "validation-succeeded.md")]
pub(crate) struct ValidationSucceeded<'a> {
    directory_changes: &'a ChangesSummary,
    services_changes: &'a HashMap<ServiceName, ChangesSummary>,
    changes_found: bool,
    invalid_base_ref_config_found: bool,
}

impl<'a> ValidationSucceeded<'a> {
    pub(crate) fn new(
        directory_changes: &'a ChangesSummary,
        services_changes: &'a HashMap<ServiceName, ChangesSummary>,
    ) -> Self {
        let changes_found = (|| {
            if !directory_changes.changes.is_empty() {
                return true;
            }
            for summary in services_changes.values() {
                if !summary.changes.is_empty() {
                    return true;
                }
            }
            false
        })();
        let invalid_base_ref_config_found = (|| {
            if directory_changes.base_ref_config_status.is_invalid() {
                return true;
            }
            for summary in services_changes.values() {
                if summary.base_ref_config_status.is_invalid() {
                    return true;
                }
            }
            false
        })();
        Self {
            directory_changes,
            services_changes,
            changes_found,
            invalid_base_ref_config_found,
        }
    }
}

mod filters {
    use anyhow::Error;
    use clowarden_core::multierror;

    /// Template filter that formats the error provided.
    pub(crate) fn format_error(err: &Error, _: &dyn askama::Values) -> askama::Result<String> {
        match multierror::format_error(err) {
            Ok(s) => Ok(s),
            Err(err) => Err(askama::Error::Custom(err.into())),
        }
    }
}

#[cfg(test)]
mod tests {
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

    const TESTDATA_PATH: &str = "src/testdata";

    fn golden_file_path(name: &str) -> String {
        format!("{TESTDATA_PATH}/templates/{name}.golden")
    }

    fn read_golden_file(name: &str) -> String {
        let path = golden_file_path(name);
        fs::read_to_string(&path).unwrap_or_else(|_| panic!("error reading golden file: {path}"))
    }

    fn write_golden_file(name: &str, content: &str) {
        let path = golden_file_path(name);
        fs::write(&path, content).expect("write golden file should succeed");
    }

    fn check_golden_file(name: &str, actual: &str) {
        if env::var("REGENERATE_GOLDEN_FILES").is_ok() {
            write_golden_file(name, actual);
        } else {
            let expected = read_golden_file(name);
            assert_eq!(actual, expected, "output does not match golden file ({name})");
        }
    }

    fn create_test_repository(name: &str) -> Repository {
        Repository {
            name: name.to_string(),
            visibility: Some(Visibility::Public),
            collaborators: None,
            teams: None,
        }
    }

    fn create_test_team(name: &str) -> Team {
        Team {
            name: name.to_string(),
            display_name: None,
            maintainers: vec![],
            members: vec![],
            annotations: HashMap::new(),
        }
    }

    #[test]
    fn test_reconciliation_completed_success_no_changes() {
        let changes_applied = HashMap::new();
        let errors = HashMap::new();

        let tmpl = ReconciliationCompleted::new(&changes_applied, &errors);
        let output = tmpl.render().unwrap();
        check_golden_file("reconciliation-completed-success-no-changes", &output);
    }

    #[test]
    fn test_reconciliation_completed_success_with_changes() {
        let mut changes_applied = HashMap::new();
        let github_changes = vec![
            ChangeApplied {
                change: Box::new(RepositoryChange::RepositoryAdded(create_test_repository(
                    "test-repo",
                ))) as DynChange,
                error: None,
                applied_at: OffsetDateTime::now_utc(),
            },
            ChangeApplied {
                change: Box::new(RepositoryChange::TeamAdded(
                    "test-repo".to_string(),
                    "engineering".to_string(),
                    Role::Write,
                )) as DynChange,
                error: None,
                applied_at: OffsetDateTime::now_utc(),
            },
            ChangeApplied {
                change: Box::new(RepositoryChange::CollaboratorAdded(
                    "test-repo".to_string(),
                    "alice".to_string(),
                    Role::Admin,
                )) as DynChange,
                error: None,
                applied_at: OffsetDateTime::now_utc(),
            },
        ];
        changes_applied.insert("github", github_changes);
        let errors = HashMap::new();

        let tmpl = ReconciliationCompleted::new(&changes_applied, &errors);
        let output = tmpl.render().unwrap();
        check_golden_file("reconciliation-completed-success-with-changes", &output);
    }

    #[test]
    fn test_reconciliation_completed_success_with_teams_and_collaborators() {
        let mut changes_applied = HashMap::new();
        let mut repo = create_test_repository("complex-repo");
        let mut teams = BTreeMap::new();
        teams.insert("backend".to_string(), Role::Admin);
        teams.insert("frontend".to_string(), Role::Write);
        repo.teams = Some(teams);
        let mut collaborators = BTreeMap::new();
        collaborators.insert("john".to_string(), Role::Maintain);
        collaborators.insert("jane".to_string(), Role::Read);
        repo.collaborators = Some(collaborators);

        let github_changes = vec![ChangeApplied {
            change: Box::new(RepositoryChange::RepositoryAdded(repo)) as DynChange,
            error: None,
            applied_at: OffsetDateTime::now_utc(),
        }];
        changes_applied.insert("github", github_changes);
        let errors = HashMap::new();

        let tmpl = ReconciliationCompleted::new(&changes_applied, &errors);
        let output = tmpl.render().unwrap();
        check_golden_file(
            "reconciliation-completed-success-with-teams-and-collaborators",
            &output,
        );
    }

    #[test]
    fn test_reconciliation_completed_with_service_errors() {
        let changes_applied = HashMap::new();
        let mut errors = HashMap::new();
        errors.insert("github", anyhow!("Failed to connect to GitHub API"));

        let tmpl = ReconciliationCompleted::new(&changes_applied, &errors);
        let output = tmpl.render().unwrap();
        check_golden_file("reconciliation-completed-with-service-errors", &output);
    }

    #[test]
    fn test_reconciliation_completed_with_change_errors() {
        let mut changes_applied = HashMap::new();
        let github_changes = vec![ChangeApplied {
            change: Box::new(RepositoryChange::TeamAdded(
                "test-repo".to_string(),
                "engineering".to_string(),
                Role::Write,
            )) as DynChange,
            error: Some("Team not found in directory".to_string()),
            applied_at: OffsetDateTime::now_utc(),
        }];
        changes_applied.insert("github", github_changes);
        let errors = HashMap::new();

        let tmpl = ReconciliationCompleted::new(&changes_applied, &errors);
        let output = tmpl.render().unwrap();
        check_golden_file("reconciliation-completed-with-change-errors", &output);
    }

    #[test]
    fn test_validation_failed_simple_error() {
        let err = anyhow!("Invalid configuration format");

        let tmpl = ValidationFailed::new(&err);
        let output = tmpl.render().unwrap();
        check_golden_file("validation-failed-simple-error", &output);
    }

    #[test]
    fn test_validation_failed_multi_error() {
        let mut merr = MultiError::new(Some("Configuration validation failed".to_string()));
        merr.push(anyhow!("Missing required field: teams"));
        merr.push(anyhow!("Invalid repository name: contains spaces"));
        let err: Error = merr.into();

        let tmpl = ValidationFailed::new(&err);
        let output = tmpl.render().unwrap();
        check_golden_file("validation-failed-multi-error", &output);
    }

    #[test]
    fn test_validation_failed_nested_error() {
        let err = anyhow!("Failed to parse configuration")
            .context("Unable to read configuration file")
            .context("Configuration validation failed");

        let tmpl = ValidationFailed::new(&err);
        let output = tmpl.render().unwrap();
        check_golden_file("validation-failed-nested-error", &output);
    }

    #[test]
    fn test_validation_succeeded_no_changes() {
        let directory_changes = ChangesSummary {
            changes: vec![],
            base_ref_config_status: BaseRefConfigStatus::Valid,
        };
        let services_changes = HashMap::new();

        let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
        let output = tmpl.render().unwrap();
        check_golden_file("validation-succeeded-no-changes", &output);
    }

    #[test]
    fn test_validation_succeeded_directory_changes() {
        let directory_changes = ChangesSummary {
            changes: vec![
                Box::new(DirectoryChange::UserAdded("Alice Smith".to_string())) as DynChange,
                Box::new(DirectoryChange::TeamAdded(create_test_team("engineering"))) as DynChange,
                Box::new(DirectoryChange::TeamMemberAdded(
                    "engineering".to_string(),
                    "alice".to_string(),
                )) as DynChange,
            ],
            base_ref_config_status: BaseRefConfigStatus::Valid,
        };
        let services_changes = HashMap::new();

        let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
        let output = tmpl.render().unwrap();
        check_golden_file("validation-succeeded-directory-changes", &output);
    }

    #[test]
    fn test_validation_succeeded_service_changes() {
        let directory_changes = ChangesSummary {
            changes: vec![],
            base_ref_config_status: BaseRefConfigStatus::Valid,
        };
        let mut services_changes = HashMap::new();
        let github_changes = ChangesSummary {
            changes: vec![
                Box::new(RepositoryChange::RepositoryAdded(create_test_repository(
                    "new-repo",
                ))) as DynChange,
                Box::new(RepositoryChange::VisibilityUpdated(
                    "existing-repo".to_string(),
                    Visibility::Private,
                )) as DynChange,
            ],
            base_ref_config_status: BaseRefConfigStatus::Valid,
        };
        services_changes.insert("github", github_changes);

        let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
        let output = tmpl.render().unwrap();
        check_golden_file("validation-succeeded-service-changes", &output);
    }

    #[test]
    fn test_validation_succeeded_directory_and_service_changes() {
        let directory_changes = ChangesSummary {
            changes: vec![Box::new(DirectoryChange::UserAdded("Bob Jones".to_string())) as DynChange],
            base_ref_config_status: BaseRefConfigStatus::Valid,
        };
        let mut services_changes = HashMap::new();
        let github_changes = ChangesSummary {
            changes: vec![Box::new(RepositoryChange::TeamRemoved(
                "old-repo".to_string(),
                "legacy-team".to_string(),
            )) as DynChange],
            base_ref_config_status: BaseRefConfigStatus::Valid,
        };
        services_changes.insert("github", github_changes);

        let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
        let output = tmpl.render().unwrap();
        check_golden_file("validation-succeeded-directory-and-service-changes", &output);
    }

    #[test]
    fn test_validation_succeeded_invalid_base_ref_directory() {
        let directory_changes = ChangesSummary {
            changes: vec![],
            base_ref_config_status: BaseRefConfigStatus::Invalid,
        };
        let services_changes = HashMap::new();

        let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
        let output = tmpl.render().unwrap();
        check_golden_file("validation-succeeded-invalid-base-ref-directory", &output);
    }

    #[test]
    fn test_validation_succeeded_invalid_base_ref_service() {
        let directory_changes = ChangesSummary {
            changes: vec![],
            base_ref_config_status: BaseRefConfigStatus::Valid,
        };
        let mut services_changes = HashMap::new();
        let github_changes = ChangesSummary {
            changes: vec![],
            base_ref_config_status: BaseRefConfigStatus::Invalid,
        };
        services_changes.insert("github", github_changes);

        let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
        let output = tmpl.render().unwrap();
        check_golden_file("validation-succeeded-invalid-base-ref-service", &output);
    }

    #[test]
    fn test_validation_succeeded_mixed_invalid_base_ref() {
        let directory_changes = ChangesSummary {
            changes: vec![Box::new(DirectoryChange::TeamRemoved("old-team".to_string())) as DynChange],
            base_ref_config_status: BaseRefConfigStatus::Invalid,
        };
        let mut services_changes = HashMap::new();
        let github_changes = ChangesSummary {
            changes: vec![Box::new(RepositoryChange::CollaboratorRoleUpdated(
                "main-repo".to_string(),
                "charlie".to_string(),
                Role::Maintain,
            )) as DynChange],
            base_ref_config_status: BaseRefConfigStatus::Valid,
        };
        services_changes.insert("github", github_changes);

        let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
        let output = tmpl.render().unwrap();
        check_golden_file("validation-succeeded-mixed-invalid-base-ref", &output);
    }

    #[test]
    fn test_validation_succeeded_team_with_members() {
        let mut team = create_test_team("devops");
        team.maintainers = vec!["alice".to_string(), "bob".to_string()];
        team.members = vec!["charlie".to_string(), "dave".to_string(), "eve".to_string()];

        let directory_changes = ChangesSummary {
            changes: vec![Box::new(DirectoryChange::TeamAdded(team)) as DynChange],
            base_ref_config_status: BaseRefConfigStatus::Valid,
        };
        let services_changes = HashMap::new();

        let tmpl = ValidationSucceeded::new(&directory_changes, &services_changes);
        let output = tmpl.render().unwrap();
        check_golden_file("validation-succeeded-team-with-members", &output);
    }
}
