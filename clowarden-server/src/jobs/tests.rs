use std::sync::{Arc, Mutex};

use anyhow::format_err;
use async_trait::async_trait;
use mockall::{Sequence, mock};
use octorust::types::ChecksCreateRequest;
use tokio::{
    sync::{Semaphore, mpsc::error::TryRecvError},
    time::Instant,
};

use clowarden_core::{
    cfg::Legacy,
    services::{
        ChangeApplied, ServiceHandler,
        github::state::{RepositoryChange, Role},
    },
};

use crate::{db::MockDB, github::MockGH};

use super::*;

const BASE_CONFIG: &str = "
teams:
  - name: team1
    maintainers:
      - alice
";
const BASE_REF: &str = "main";
const BASE_REF_WARNING: &str = "The configuration in the base reference could not be read or is not valid";
const CHECK_FAILED: &str = "The configuration changes proposed are not valid";
const CHECK_SUCCEEDED: &str = "The configuration changes proposed are valid";
const COMMENT_NOT_POSTED: &str = "(the validation comment could not be posted)";
const CONFIG_PATH: &str = "config.yaml";
const DIRECTORY_CHANGE: &str = "- team **team2** has been *added*";
const FORK_OWNER: &str = "fork-owner";
const FORK_REPO: &str = "fork-repo";
const GITHUB: ServiceName = "github";
const GITHUB_ERROR: &str = "github is down";
const HEAD_CONFIG: &str = "
teams:
  - name: team1
    maintainers:
      - alice
  - name: team2
    maintainers:
      - bob
";
const HEAD_REF: &str = "patch-1";
const HEAD_SHA: &str = "abc123";
const INSTALLATION_ID: i64 = 1;
const ONE_HOUR: Duration = Duration::from_secs(3600);
const ORG: &str = "cncf";
const OTHER_ORG: &str = "other-org";
const OTHER_SERVICE: ServiceName = "other";
const PR_NUMBER: i64 = 42;
const REPO: &str = "clowarden-config";
const SERVICE_CHANGE: &str =
    "- user **carol** is now a collaborator (role: **write**) of repository **repo1**";
const SERVICE_ERROR: &str = "invalid github service configuration";

#[tokio::test]
async fn test_handle_reconcile_job_comment_failure_is_best_effort() {
    // Setup service expectations
    let mut svc = MockService::new();
    svc.expect_reconcile().times(1).returning(|_| Ok(vec![]));

    // Setup audit and comment expectations
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(GITHUB_ERROR)))));

    // Run the job
    let worker = setup_worker(db, gh, Arc::new(FakeCoreGH::default()), [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_reconcile_job(setup_reconcile_input(Some(PR_NUMBER))).await;

    // Check the job succeeds
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_reconcile_job_db_failure_is_best_effort() {
    // Setup service expectations
    let mut svc = MockService::new();
    svc.expect_reconcile().times(1).returning(|_| Ok(vec![]));

    // Setup audit and comment expectations
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!("database is down")))));
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|ctx, pr_number, body| {
            is_org_ctx(ctx) && *pr_number == PR_NUMBER && body.contains("No actionable changes detected.")
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(1))));

    // Run the job
    let worker = setup_worker(db, gh, Arc::new(FakeCoreGH::default()), [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_reconcile_job(setup_reconcile_input(Some(PR_NUMBER))).await;

    // Check the job succeeds
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_reconcile_job_partial_failure_reaches_audit_and_comment() {
    // Setup services expectations
    let mut github_svc = MockService::new();
    github_svc
        .expect_reconcile()
        .withf(is_org)
        .times(1)
        .returning(|_| Ok(vec![setup_change_applied()]));
    let mut other_svc = MockService::new();
    other_svc
        .expect_reconcile()
        .withf(is_org)
        .times(1)
        .returning(|_| Err(format_err!(SERVICE_ERROR)));

    // Setup audit and comment expectations
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .withf(|input, changes_applied, errors| {
            *input == setup_reconcile_input(Some(PR_NUMBER))
                && changes_applied.len() == 1
                && changes_applied.get(GITHUB).is_some_and(|changes| changes.len() == 1)
                && errors.len() == 1
                && errors.get(OTHER_SERVICE).is_some_and(|err| err.to_string() == SERVICE_ERROR)
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|ctx, pr_number, body| {
            is_org_ctx(ctx)
                && *pr_number == PR_NUMBER
                && body.contains("Some errors were found during the reconciliation")
                && body.contains(SERVICE_ERROR)
                && !body.contains("all changes have been applied")
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(1))));

    // Run the job
    let worker = setup_worker(
        db,
        gh,
        Arc::new(FakeCoreGH::default()),
        [
            (GITHUB, Arc::new(github_svc)),
            (OTHER_SERVICE, Arc::new(other_svc)),
        ],
    );
    let result = worker.handle_reconcile_job(setup_reconcile_input(Some(PR_NUMBER))).await;

    // Check the job succeeds, as the failure has already been reported
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_reconcile_job_periodic_records_changes_without_commenting() {
    // Setup service expectations
    let mut svc = MockService::new();
    svc.expect_reconcile()
        .withf(is_org)
        .times(1)
        .returning(|_| Ok(vec![setup_change_applied()]));

    // Setup audit and comment expectations
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .withf(|input, changes_applied, errors| {
            *input == setup_reconcile_input(None)
                && changes_applied.get(GITHUB).is_some_and(|changes| changes.len() == 1)
                && errors.is_empty()
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    let mut gh = MockGH::new();
    gh.expect_post_comment().never();

    // Run the job
    let worker = setup_worker(db, gh, Arc::new(FakeCoreGH::default()), [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_reconcile_job(setup_reconcile_input(None)).await;

    // Check the job succeeds
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_reconcile_job_periodic_records_service_error_without_commenting() {
    // Setup service expectations
    let mut svc = MockService::new();
    svc.expect_reconcile()
        .withf(is_org)
        .times(1)
        .returning(|_| Err(format_err!(SERVICE_ERROR)));

    // Setup audit and comment expectations
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .withf(|input, changes_applied, errors| {
            *input == setup_reconcile_input(None)
                && changes_applied.is_empty()
                && errors.get(GITHUB).is_some_and(|err| err.to_string() == SERVICE_ERROR)
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    let mut gh = MockGH::new();
    gh.expect_post_comment().never();

    // Run the job
    let worker = setup_worker(db, gh, Arc::new(FakeCoreGH::default()), [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_reconcile_job(setup_reconcile_input(None)).await;

    // Check the job succeeds
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_validate_job_check_failure_is_returned() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_REF, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment().times(1).returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run()
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(GITHUB_ERROR)))));

    // Run the job
    let worker = setup_worker(MockDB::new(), gh, ghc, [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_validate_job(setup_validate_input()).await;

    // Check the job fails with the GitHub error
    assert_eq!(result.unwrap_err().to_string(), GITHUB_ERROR);
}

#[tokio::test]
async fn test_handle_validate_job_comment_failure_is_noted_in_failed_check() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_REF, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _| Err(format_err!(SERVICE_ERROR)));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(GITHUB_ERROR)))));
    gh.expect_create_check_run()
        .withf(|ctx, body| {
            let msg = format!("{CHECK_FAILED} {COMMENT_NOT_POSTED}");
            is_org_ctx(ctx) && is_pr_head_check(body, &ChecksCreateRequestConclusion::Failure, &msg)
        })
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Run the job
    let worker = setup_worker(MockDB::new(), gh, ghc, [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_validate_job(setup_validate_input()).await;

    // Check the job fails with the validation error, not with the GitHub one
    let err = result.unwrap_err().to_string();
    assert!(err.contains(SERVICE_ERROR), "unexpected error: {err}");
    assert!(!err.contains(GITHUB_ERROR), "unexpected error: {err}");
}

#[tokio::test]
async fn test_handle_validate_job_comment_failure_is_noted_in_successful_check() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_REF, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(GITHUB_ERROR)))));
    gh.expect_create_check_run()
        .withf(|ctx, body| {
            let msg = format!("{CHECK_SUCCEEDED} {COMMENT_NOT_POSTED}");
            is_org_ctx(ctx) && is_pr_head_check(body, &ChecksCreateRequestConclusion::Success, &msg)
        })
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Run the job
    let worker = setup_worker(MockDB::new(), gh, ghc, [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_validate_job(setup_validate_input()).await;

    // Check the job succeeds, as the check run has been completed
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_validate_job_fork_head_is_read_from_fork() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_REF, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .withf(|org, head_src| {
            is_org(org)
                && head_src.inst_id == Some(INSTALLATION_ID)
                && head_src.owner == FORK_OWNER
                && head_src.repo == FORK_REPO
                && head_src.ref_ == HEAD_REF
        })
        .times(1)
        .returning(|_, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|_, _, body| body.contains(DIRECTORY_CHANGE))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run()
        .withf(|_, body| is_success_check(body))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Run the job
    let worker = setup_worker(MockDB::new(), gh, ghc.clone(), [(GITHUB, Arc::new(svc))]);
    let input = ValidateInput {
        pr_head_owner: Some(FORK_OWNER.to_string()),
        pr_head_repo: Some(FORK_REPO.to_string()),
        ..setup_validate_input()
    };
    let result = worker.handle_validate_job(input).await;

    // Check head config was read from the fork and base config from the org
    assert!(result.is_ok());
    assert_fetches(
        &ghc,
        vec![
            Fetch::new(FORK_OWNER, FORK_REPO, HEAD_REF),
            Fetch::new(ORG, REPO, BASE_REF),
        ],
    );
}

#[tokio::test]
async fn test_handle_validate_job_head_without_repo_is_read_from_org() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_REF, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .withf(|_, head_src| head_src.owner == ORG && head_src.repo == REPO && head_src.ref_ == HEAD_REF)
        .times(1)
        .returning(|_, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|_, _, body| body.contains(DIRECTORY_CHANGE))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run()
        .withf(|_, body| is_success_check(body))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Run the job
    let worker = setup_worker(MockDB::new(), gh, ghc.clone(), [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_validate_job(setup_validate_input()).await;

    // Check both configs were read from the org
    assert!(result.is_ok());
    assert_fetches(
        &ghc,
        vec![Fetch::new(ORG, REPO, HEAD_REF), Fetch::new(ORG, REPO, BASE_REF)],
    );
}

#[tokio::test]
async fn test_handle_validate_job_service_error_fails_check() {
    // Setup configuration and services expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_REF, Ok(HEAD_CONFIG))]);
    let mut github_svc = MockService::new();
    github_svc
        .expect_get_changes_summary()
        .times(1)
        .returning(|_, _| Err(format_err!(SERVICE_ERROR)));
    let mut other_svc = MockService::new();
    other_svc
        .expect_get_changes_summary()
        .times(1)
        .returning(|_, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));

    // Setup GitHub expectations
    let mut seq = Sequence::new();
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|ctx, pr_number, body| {
            is_org_ctx(ctx)
                && *pr_number == PR_NUMBER
                && body.contains("## Validation failed")
                && body.contains(SERVICE_ERROR)
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run()
        .withf(|ctx, body| is_org_ctx(ctx) && is_failure_check(body))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Run the job
    let worker = setup_worker(
        MockDB::new(),
        gh,
        ghc,
        [
            (GITHUB, Arc::new(github_svc)),
            (OTHER_SERVICE, Arc::new(other_svc)),
        ],
    );
    let result = worker.handle_validate_job(setup_validate_input()).await;

    // Check the job fails with the validation error
    let err = result.unwrap_err().to_string();
    assert!(err.contains(SERVICE_ERROR), "unexpected error: {err}");
}

#[tokio::test]
async fn test_handle_validate_job_services_errors_are_aggregated() {
    // Setup configuration and services expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_REF, Ok(HEAD_CONFIG))]);
    let mut github_svc = MockService::new();
    github_svc
        .expect_get_changes_summary()
        .times(1)
        .returning(|_, _| Err(format_err!("first service error")));
    let mut other_svc = MockService::new();
    other_svc
        .expect_get_changes_summary()
        .times(1)
        .returning(|_, _| Err(format_err!("second service error")));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|_, _, body| body.contains("first service error") && body.contains("second service error"))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run()
        .withf(|_, body| is_failure_check(body))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Run the job
    let worker = setup_worker(
        MockDB::new(),
        gh,
        ghc,
        [
            (GITHUB, Arc::new(github_svc)),
            (OTHER_SERVICE, Arc::new(other_svc)),
        ],
    );
    let result = worker.handle_validate_job(setup_validate_input()).await;

    // Check the job fails with both errors
    let err = result.unwrap_err().to_string();
    assert!(err.contains("first service error"), "unexpected error: {err}");
    assert!(err.contains("second service error"), "unexpected error: {err}");
}

#[tokio::test]
async fn test_handle_validate_job_success_reports_on_pr_head() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_REF, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary().times(1).returning(|_, _| {
        let change =
            RepositoryChange::CollaboratorAdded("repo1".to_string(), "carol".to_string(), Role::Write);
        Ok(setup_summary(vec![Box::new(change)], BaseRefConfigStatus::Valid))
    });

    // Setup GitHub expectations
    let mut seq = Sequence::new();
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|ctx, pr_number, body| {
            is_org_ctx(ctx)
                && *pr_number == PR_NUMBER
                && body.contains("## Validation succeeded")
                && body.contains(DIRECTORY_CHANGE)
                && body.contains(SERVICE_CHANGE)
        })
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run()
        .withf(|ctx, body| is_org_ctx(ctx) && is_success_check(body))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Run the job
    let worker = setup_worker(MockDB::new(), gh, ghc, [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_validate_job(setup_validate_input()).await;

    // Check the job succeeds
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_validate_job_unfetchable_head_skips_services() {
    let head = Err("file not found");
    check_invalid_head_skips_services(head, "error getting permissions file").await;
}

#[tokio::test]
async fn test_handle_validate_job_unparsable_head_skips_services() {
    let head = Ok("teams: [");
    check_invalid_head_skips_services(head, "error parsing permissions file").await;
}

#[tokio::test]
async fn test_handle_validate_job_unreadable_base_passes_with_warning() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Err("file not found")), (HEAD_REF, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Invalid)));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(|_, _, body| {
            body.matches(BASE_REF_WARNING).count() == 2 && !body.contains("No actionable changes detected.")
        })
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run()
        .withf(|_, body| is_success_check(body))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Run the job
    let worker = setup_worker(MockDB::new(), gh, ghc, [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_validate_job(setup_validate_input()).await;

    // Check the job succeeds, so that a broken base config can be repaired
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_validate_job_validation_error_outlives_github_failures() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_REF, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _| Err(format_err!(SERVICE_ERROR)));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Err(format_err!(GITHUB_ERROR)))));
    gh.expect_create_check_run()
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(GITHUB_ERROR)))));

    // Run the job
    let worker = setup_worker(MockDB::new(), gh, ghc, [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_validate_job(setup_validate_input()).await;

    // Check the job fails with the validation error, not with the GitHub one
    let err = result.unwrap_err().to_string();
    assert!(err.contains(SERVICE_ERROR), "unexpected error: {err}");
    assert!(!err.contains(GITHUB_ERROR), "unexpected error: {err}");
}

#[tokio::test(start_paused = true)]
async fn test_handler_runs_jobs_serially_within_org_and_concurrently_across_orgs() {
    // Setup a service that blocks each job until it is released
    let (svc, mut started_rx) = GatedService::new([ORG, OTHER_ORG]);
    let services = HashMap::from([(GITHUB, svc.clone() as DynServiceHandler)]);

    // Setup audit expectations
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .times(3)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Start the handler
    let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
    let cancel_token = CancellationToken::new();
    let workers = handler(
        &(Arc::new(db) as DynDB),
        &(Arc::new(MockGH::new()) as DynGH),
        &(Arc::new(FakeCoreGH::default()) as core::github::DynGH),
        &services,
        jobs_rx,
        cancel_token.clone(),
        &vec![setup_org(ORG), setup_org(OTHER_ORG)],
    );

    // Send some jobs, including one for an organization without a worker
    for org_name in ["unknown-org", ORG, ORG, OTHER_ORG] {
        jobs_tx.send(setup_periodic_job(setup_org(org_name))).unwrap();
    }

    // Check the first job of each organization are running at the same time
    let mut started = vec![
        next_started(&mut started_rx).await,
        next_started(&mut started_rx).await,
    ];
    started.sort();
    assert_eq!(started, vec![ORG, OTHER_ORG]);

    // Check the second job of the organization waits for the first one
    assert_nothing_else_starts(&mut started_rx).await;
    svc.release(ORG);
    assert_eq!(next_started(&mut started_rx).await, ORG);
    assert_nothing_else_starts(&mut started_rx).await;

    // Finish the jobs and stop the handler
    svc.release(ORG);
    svc.release(OTHER_ORG);
    cancel_token.cancel();
    assert!(workers.await.into_iter().all(|result| result.is_ok()));

    // Check the job for the unknown organization did not run
    assert_eq!(started_rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn test_org_worker_run_finishes_queued_jobs_after_cancellation() {
    // Setup service expectations
    let mut svc = MockService::new();
    svc.expect_reconcile().times(2).returning(|_| Ok(vec![]));

    // Setup audit and comment expectations, one job after the other
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    let mut gh = MockGH::new();
    for job_pr_number in [1, 2] {
        db.expect_register_reconciliation()
            .withf(move |input, _, _| input.pr_number == Some(job_pr_number))
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
        gh.expect_post_comment()
            .withf(move |_, pr_number, _| *pr_number == job_pr_number)
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    }

    // Queue the jobs and cancel the worker before it starts
    let (org_jobs_tx, org_jobs_rx) = mpsc::unbounded_channel();
    for pr_number in [1, 2] {
        org_jobs_tx.send(Job::Reconcile(setup_reconcile_input(Some(pr_number)))).unwrap();
    }
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    // Run the worker
    let worker = setup_worker(db, gh, Arc::new(FakeCoreGH::default()), [(GITHUB, Arc::new(svc))]);
    let result = worker.run(org_jobs_rx, cancel_token).await;

    // Check the worker exits after completing the queued jobs
    assert!(result.is_ok());
}

#[tokio::test(start_paused = true)]
async fn test_org_worker_run_finishes_running_job_after_cancellation() {
    // Setup a service that blocks the job until it is released
    let (svc, mut started_rx) = GatedService::new([ORG]);

    // Setup audit expectations
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .withf(|input, _, _| input.pr_number.is_none())
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Start the worker and wait for the job to be running
    let (org_jobs_tx, org_jobs_rx) = mpsc::unbounded_channel();
    org_jobs_tx.send(setup_periodic_job(setup_org(ORG))).unwrap();
    let cancel_token = CancellationToken::new();
    let worker = setup_worker(
        db,
        MockGH::new(),
        Arc::new(FakeCoreGH::default()),
        [(GITHUB, svc.clone())],
    );
    let mut worker_done = worker.run(org_jobs_rx, cancel_token.clone());
    assert_eq!(next_started(&mut started_rx).await, ORG);

    // Check the worker keeps waiting for the job after being cancelled
    cancel_token.cancel();
    let still_running = timeout(Duration::from_secs(1), &mut worker_done).await;
    assert!(still_running.is_err(), "worker exited: {still_running:?}");

    // Check the worker exits once the job has reached the database
    svc.release(ORG);
    assert!(worker_done.await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn test_org_worker_run_moves_on_after_reconcile_timeout() {
    let hanging_job = Job::Reconcile(setup_reconcile_input(None));
    check_worker_moves_on_after_timeout(hanging_job).await;
}

#[tokio::test(start_paused = true)]
async fn test_org_worker_run_moves_on_after_validate_timeout() {
    let hanging_job = Job::Validate(setup_validate_input());
    check_worker_moves_on_after_timeout(hanging_job).await;
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_enqueues_every_org_on_each_cycle() {
    // Start the scheduler
    let orgs = [setup_org(ORG), setup_org(OTHER_ORG)];
    let (jobs_tx, mut jobs_rx) = mpsc::unbounded_channel();
    let cancel_token = CancellationToken::new();
    let start = Instant::now();
    let scheduler_done = scheduler(jobs_tx, cancel_token.clone(), &orgs);

    // Check the jobs of the first cycle are spaced out and have no PR data
    for org in &orgs {
        assert_eq!(jobs_rx.recv().await, Some(setup_periodic_job(org.clone())));
    }
    assert_eq!(start.elapsed(), Duration::from_secs(RECONCILE_STAGGER));

    // Check the next cycle enqueues the same jobs when it is due
    for org in &orgs {
        assert_eq!(jobs_rx.recv().await, Some(setup_periodic_job(org.clone())));
    }
    assert_eq!(start.elapsed(), ONE_HOUR + Duration::from_secs(RECONCILE_STAGGER));

    // Stop the scheduler
    cancel_token.cancel();
    assert!(scheduler_done.await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_sends_no_jobs_when_already_cancelled() {
    // Setup a token that is already cancelled
    let (jobs_tx, mut jobs_rx) = mpsc::unbounded_channel();
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    // Run the scheduler
    let results = scheduler(jobs_tx, cancel_token, &[setup_org(ORG)]).await;

    // Check it exited without sending any job
    assert!(results.into_iter().all(|result| result.is_ok()));
    assert_eq!(jobs_rx.recv().await, None);
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_stops_during_stagger_when_cancelled() {
    // Start the scheduler
    let orgs = [setup_org(ORG), setup_org(OTHER_ORG), setup_org("third-org")];
    let (jobs_tx, mut jobs_rx) = mpsc::unbounded_channel();
    let cancel_token = CancellationToken::new();
    let start = Instant::now();
    let scheduler_done = scheduler(jobs_tx, cancel_token.clone(), &orgs);

    // Wait for the first job and cancel while the scheduler waits to send the next one
    assert_eq!(jobs_rx.recv().await, Some(setup_periodic_job(orgs[0].clone())));
    cancel_token.cancel();
    assert!(scheduler_done.await.into_iter().all(|result| result.is_ok()));

    // Check it exited right away without sending any other job
    assert_eq!(start.elapsed(), Duration::ZERO);
    assert_eq!(jobs_rx.recv().await, None);
}

// Helpers.

/// Check the fetches received match the ones expected, regardless of their
/// order, as what matters is where the configuration is read from.
fn assert_fetches(ghc: &FakeCoreGH, mut expected: Vec<Fetch>) {
    let mut fetches = ghc.fetches();
    fetches.sort();
    expected.sort();
    assert_eq!(fetches, expected);
}

/// Check no other service call starts. The clock is paused in the tests using
/// this helper, so the timeout only expires once every task is idle.
async fn assert_nothing_else_starts(started_rx: &mut mpsc::UnboundedReceiver<String>) {
    let started = timeout(Duration::from_secs(1), started_rx.recv()).await;
    assert!(started.is_err(), "unexpected service call: {started:?}");
}

/// Check that a validate job whose head configuration cannot be loaded fails
/// with the error expected, without involving any of the services.
async fn check_invalid_head_skips_services(
    head: Result<&'static str, &'static str>,
    expected_err: &'static str,
) {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_REF, head)]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary().never();

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .withf(move |_, _, body| body.contains("## Validation failed") && body.contains(expected_err))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run()
        .withf(|_, body| is_failure_check(body))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Run the job
    let worker = setup_worker(MockDB::new(), gh, ghc, [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_validate_job(setup_validate_input()).await;

    // Check the job fails with the directory error
    let err = result.unwrap_err().to_string();
    assert!(err.contains(expected_err), "unexpected error: {err}");
}

/// Check that a job hanging in its service is timed out, so that the worker can
/// process the next one. The outcome of the job timed out is not relevant here,
/// so the expectations of its side effects accept it being reported or not.
async fn check_worker_moves_on_after_timeout(hanging_job: Job) {
    // Setup a service that hangs until it is released
    let (svc, mut started_rx) = GatedService::new([ORG]);

    // Setup configuration, audit and GitHub expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_REF, Ok(HEAD_CONFIG))]);
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .times(1..=2)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    let mut gh = MockGH::new();
    gh.expect_post_comment().times(0..=1).returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run()
        .times(0..=1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Queue the jobs and start the worker
    let (org_jobs_tx, org_jobs_rx) = mpsc::unbounded_channel();
    org_jobs_tx.send(hanging_job).unwrap();
    org_jobs_tx.send(setup_periodic_job(setup_org(ORG))).unwrap();
    let cancel_token = CancellationToken::new();
    let worker = setup_worker(db, gh, ghc, [(GITHUB, svc.clone())]);
    let worker_done = worker.run(org_jobs_rx, cancel_token.clone());

    // Check the second job starts once the first one times out
    assert_eq!(next_started(&mut started_rx).await, ORG);
    let first_job_started_at = Instant::now();
    assert_eq!(next_started(&mut started_rx).await, ORG);
    assert_eq!(first_job_started_at.elapsed(), ONE_HOUR);

    // Finish the second job and stop the worker
    svc.release(ORG);
    cancel_token.cancel();
    assert!(worker_done.await.is_ok());
}

/// Check if the check run provided fails the PR head.
fn is_failure_check(body: &ChecksCreateRequest) -> bool {
    is_pr_head_check(body, &ChecksCreateRequestConclusion::Failure, CHECK_FAILED)
}

/// Check if the organization provided is the test one.
fn is_org(org: &Organization) -> bool {
    *org == setup_org(ORG)
}

/// Check if the context provided targets the test organization's repository.
fn is_org_ctx(ctx: &Ctx) -> bool {
    ctx.inst_id == INSTALLATION_ID && ctx.owner == ORG && ctx.repo == REPO
}

/// Check if the check run provided completes the PR head with the conclusion
/// and message expected.
fn is_pr_head_check(
    body: &ChecksCreateRequest,
    conclusion: &ChecksCreateRequestConclusion,
    msg: &str,
) -> bool {
    body.conclusion.as_ref() == Some(conclusion)
        && body.head_sha == HEAD_SHA
        && body.name == "CLOWarden"
        && body.output.as_ref().is_some_and(|output| output.summary == msg && output.title == msg)
        && body.status == Some(JobStatus::Completed)
}

/// Check if the check run provided passes the PR head.
fn is_success_check(body: &ChecksCreateRequest) -> bool {
    is_pr_head_check(body, &ChecksCreateRequestConclusion::Success, CHECK_SUCCEEDED)
}

/// Wait for the next service call, failing if the worker never gets to it.
async fn next_started(started_rx: &mut mpsc::UnboundedReceiver<String>) -> String {
    timeout(Duration::from_secs(2 * JOB_TIMEOUT), started_rx.recv())
        .await
        .expect("service call should start")
        .expect("service should be alive")
}

/// Create a successful change applied.
fn setup_change_applied() -> ChangeApplied {
    ChangeApplied {
        change: Box::new(RepositoryChange::TeamRemoved(
            "repo1".to_string(),
            "team1".to_string(),
        )),
        error: None,
        applied_at: OffsetDateTime::UNIX_EPOCH,
    }
}

/// Create an organization with the name provided.
fn setup_org(name: &str) -> Organization {
    Organization {
        name: name.to_string(),
        installation_id: INSTALLATION_ID,
        repository: REPO.to_string(),
        branch: BASE_REF.to_string(),
        legacy: Legacy {
            enabled: true,
            sheriff_permissions_path: CONFIG_PATH.to_string(),

            cncf_people_path: None,
        },
    }
}

/// Create the job the scheduler is expected to send for the organization.
fn setup_periodic_job(org: Organization) -> Job {
    Job::Reconcile(ReconcileInput {
        org,
        ..Default::default()
    })
}

/// Create the input of a reconcile job.
fn setup_reconcile_input(pr_number: Option<i64>) -> ReconcileInput {
    ReconcileInput {
        org: setup_org(ORG),
        pr_number,
        ..Default::default()
    }
}

/// Create a changes summary.
fn setup_summary(changes: Vec<core::services::DynChange>, status: BaseRefConfigStatus) -> ChangesSummary {
    ChangesSummary {
        changes,
        base_ref_config_status: status,
        warnings: vec![],
    }
}

/// Create the input of a validate job for a PR not coming from a fork.
fn setup_validate_input() -> ValidateInput {
    ValidateInput {
        org: setup_org(ORG),
        pr_number: PR_NUMBER,
        pr_head_owner: None,
        pr_head_repo: None,
        pr_head_ref: HEAD_REF.to_string(),
        pr_head_sha: HEAD_SHA.to_string(),
    }
}

/// Create an organization worker using the dependencies provided.
fn setup_worker<const N: usize>(
    db: MockDB,
    gh: MockGH,
    ghc: Arc<FakeCoreGH>,
    services: [(ServiceName, DynServiceHandler); N],
) -> OrgWorker {
    OrgWorker::new(Arc::new(db), Arc::new(gh), ghc, HashMap::from(services))
}

/// Core GH fake that serves the content configured for each reference and
/// records the fetches received. Core's MockGH is only available to its tests,
/// and Source is neither Clone nor Debug, so the fetches are recorded as Fetch.
#[derive(Default)]
struct FakeCoreGH {
    fetches: Mutex<Vec<Fetch>>,
    files: HashMap<&'static str, Result<&'static str, &'static str>>,
}

impl FakeCoreGH {
    /// Create a new fake that serves the content or error of each reference.
    fn new<const N: usize>(files: [(&'static str, Result<&'static str, &'static str>); N]) -> Arc<Self> {
        Arc::new(Self {
            fetches: Mutex::default(),
            files: HashMap::from(files),
        })
    }

    /// Return the fetches received so far.
    fn fetches(&self) -> Vec<Fetch> {
        self.fetches.lock().unwrap().clone()
    }
}

#[async_trait]
impl core::github::GH for FakeCoreGH {
    /// [core::github::GH::get_file_content]
    async fn get_file_content(&self, src: &Source, path: &str) -> Result<String> {
        assert_eq!(src.inst_id, Some(INSTALLATION_ID));
        assert_eq!(path, CONFIG_PATH);
        self.fetches.lock().unwrap().push(Fetch::new(&src.owner, &src.repo, &src.ref_));
        match self.files.get(src.ref_.as_str()) {
            Some(Ok(content)) => Ok((*content).to_string()),
            Some(Err(err)) => Err(format_err!(*err)),
            None => Err(format_err!("unexpected reference: {}", src.ref_)),
        }
    }
}

/// A file fetch received by the core GH fake.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Fetch {
    owner: String,
    repo: String,
    ref_: String,
}

impl Fetch {
    /// Create a new fetch instance.
    fn new(owner: &str, repo: &str, ref_: &str) -> Self {
        Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            ref_: ref_.to_string(),
        }
    }
}

/// Service handler that reports when it is called with an organization and
/// then waits until it is released, as mockall cannot block a call.
struct GatedService {
    gates: HashMap<String, Semaphore>,
    started_tx: mpsc::UnboundedSender<String>,
}

impl GatedService {
    /// Create a new service for the organizations provided, returning it along
    /// with the receiver of the names of the organizations of the calls started.
    fn new<const N: usize>(org_names: [&str; N]) -> (Arc<Self>, mpsc::UnboundedReceiver<String>) {
        let (started_tx, started_rx) = mpsc::unbounded_channel();
        let gates = org_names.iter().map(|name| ((*name).to_string(), Semaphore::new(0))).collect();
        (Arc::new(Self { gates, started_tx }), started_rx)
    }

    /// Report the call and wait until it is released.
    async fn enter(&self, org: &Organization) {
        self.started_tx.send(org.name.clone()).unwrap();
        self.gates[&org.name].acquire().await.unwrap().forget();
    }

    /// Allow one of the calls for the organization provided to finish.
    fn release(&self, org_name: &str) {
        self.gates[org_name].add_permits(1);
    }
}

#[async_trait]
impl ServiceHandler for GatedService {
    /// [ServiceHandler::get_changes_summary]
    async fn get_changes_summary(&self, org: &Organization, _head_src: &Source) -> Result<ChangesSummary> {
        self.enter(org).await;
        Ok(setup_summary(vec![], BaseRefConfigStatus::Valid))
    }

    /// [ServiceHandler::reconcile]
    async fn reconcile(&self, org: &Organization) -> Result<ChangesApplied> {
        self.enter(org).await;
        Ok(vec![])
    }
}

mock! {
    Service {}

    #[async_trait]
    impl ServiceHandler for Service {
        async fn get_changes_summary(&self, org: &Organization, head_src: &Source) -> Result<ChangesSummary>;
        async fn reconcile(&self, org: &Organization) -> Result<ChangesApplied>;
    }
}
