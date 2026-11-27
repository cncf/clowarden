use std::sync::{Arc, Mutex};

use anyhow::{Result, format_err};
use async_trait::async_trait;
use futures::future;
use mockall::{Sequence, mock};
use octorust::types::{ChecksCreateRequest, ChecksCreateRequestConclusion, JobStatus};
use tokio::{
    sync::{Semaphore, mpsc::error::TryRecvError},
    time::{Instant, timeout},
};

use clowarden_core::{
    cfg::Legacy,
    github::{RateLimitError, Source, is_rate_limit_error},
    services::{
        BaseRefConfigStatus, ChangeApplied, ChangesSummary, JobContext, ReconcileOutcome, ServiceHandler,
        github::state::{RepositoryChange, Role},
    },
};

use crate::{
    db::MockDB,
    github::{self, Ctx, MockGH, PrFiles},
};

use super::{
    checks::{CheckRunPublisher, CheckRunTarget},
    triage::NO_CONFIG_CHANGES_MSG,
    worker::{
        INTERRUPTED_VALIDATION_MSG, JOB_HARD_TIMEOUT_GRACE, JOB_TIMEOUT, OrgWorker, OrgWorkerActivity,
        RATE_LIMITED_VALIDATION_MSG, ValidateErrorKind,
    },
    *,
};

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
async fn test_execute_validate_job_classifies_config_interrupted_and_operational_errors() {
    // 1. Ordinary invalid YAML -> ConfigInvalid (logged at info, not error)
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(":::invalid"))]);
    let mut gh = MockGH::new();
    gh.expect_post_comment().times(1).returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run().times(1).returning(|_, _| Box::pin(future::ready(Ok(()))));
    let worker = setup_worker(MockDB::new(), gh, ghc, []);
    let err = worker
        .execute_validate_job(setup_validate_input(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ValidateErrorKind::ConfigInvalid);

    // 2. Service error containing octorust::ClientError -> Operational
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary().times(1).returning(|_, _, _| {
        Err(Error::new(octorust::ClientError::SerdeJsonError(
            serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
        )))
    });
    let mut gh = MockGH::new();
    gh.expect_post_comment().times(1).returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run().times(1).returning(|_, _| Box::pin(future::ready(Ok(()))));
    let worker = setup_worker(MockDB::new(), gh, ghc, [(GITHUB, Arc::new(svc))]);
    let err = worker
        .execute_validate_job(setup_validate_input(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ValidateErrorKind::Operational);

    // 3. Rate-limited interruption during validation -> Interrupted
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary().times(1).returning(|_, _, _| {
        Err(Error::new(RateLimitError::primary(
            Some(Duration::from_secs(10)),
            "primary limit",
        )))
    });
    let worker = setup_worker(MockDB::new(), MockGH::new(), ghc, [(GITHUB, Arc::new(svc))]);
    let err = worker
        .execute_validate_job(setup_validate_input(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ValidateErrorKind::Interrupted);

    // 4. Valid PR whose check-run publication fails with rate limit during shutdown -> Operational
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));
    let mut gh = MockGH::new();
    gh.expect_post_comment().times(1).returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run().times(1).returning(|_, _| {
        Box::pin(future::ready(Err(Error::new(RateLimitError::primary(
            Some(Duration::from_secs(10)),
            "primary limit",
        )))))
    });
    let stop_intake = CancellationToken::new();
    stop_intake.cancel();
    let worker = setup_worker_with_tokens(
        MockDB::new(),
        gh,
        ghc,
        [(GITHUB, Arc::new(svc))],
        stop_intake,
        CancellationToken::new(),
    );
    let err = worker
        .execute_validate_job(setup_validate_input(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ValidateErrorKind::Operational);
}

#[tokio::test]
async fn test_handle_pr_triage_changed_opened_creates_in_progress_check_and_enqueues_validate() {
    let mut gh = MockGH::new();
    gh.expect_list_pr_files()
        .withf(|ctx, pr_number| is_org_ctx(ctx) && *pr_number == PR_NUMBER)
        .times(1)
        .returning(|_, _| {
            Box::pin(future::ready(Ok(PrFiles {
                files: vec!["docs/README.md".to_string(), CONFIG_PATH.to_string()],
                is_truncated: false,
            })))
        });
    gh.expect_create_check_run()
        .withf(|ctx, body| is_org_ctx(ctx) && is_in_progress_check(body))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    setup_triage_ctx(gh, pr_tx)
        .handle_pr_triage(setup_triage_input(PullRequestEventAction::Opened))
        .await;

    assert_eq!(pr_rx.try_recv(), Ok(Job::Validate(setup_validate_input())));
}

#[tokio::test]
async fn test_handle_pr_triage_closed_cancelled_does_not_enqueue_reconcile() {
    // Setup a merged PR triage whose file lookup is cancelled by shutdown
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().times(1).returning(|_, _| {
        Box::pin(future::ready(Err(Error::new(
            clowarden_core::github::CancelledError,
        ))))
    });

    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let triage_ctx = setup_triage_ctx(gh, pr_tx);
    triage_ctx.cancel_jobs.cancel();
    triage_ctx.handle_pr_triage(setup_triage_input(PullRequestEventAction::Closed)).await;

    // Check cancellation did not enqueue a guaranteed-to-fail reconcile job
    assert_eq!(pr_rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn test_handle_pr_triage_closed_merged_enqueues_reconcile_without_check() {
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().times(1).returning(|_, _| {
        Box::pin(future::ready(Ok(PrFiles {
            files: vec![CONFIG_PATH.to_string()],
            is_truncated: false,
        })))
    });
    gh.expect_create_check_run().never();

    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    setup_triage_ctx(gh, pr_tx)
        .handle_pr_triage(setup_triage_input(PullRequestEventAction::Closed))
        .await;

    assert_eq!(
        pr_rx.try_recv(),
        Ok(Job::Reconcile(setup_reconcile_input(Some(PR_NUMBER))))
    );
}

#[tokio::test]
async fn test_handle_pr_triage_closed_merged_list_files_failure_enqueues_reconcile() {
    // Setup GitHub failing list_pr_files on a merged PR
    let mut gh = MockGH::new();
    gh.expect_list_pr_files()
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(GITHUB_ERROR)))));
    gh.expect_create_check_run().never();

    // Run triage for the merged PR
    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    setup_triage_ctx(gh, pr_tx)
        .handle_pr_triage(setup_triage_input(PullRequestEventAction::Closed))
        .await;

    // Check reconcile was enqueued defensively despite the file-list failure
    assert_eq!(
        pr_rx.try_recv(),
        Ok(Job::Reconcile(setup_reconcile_input(Some(PR_NUMBER))))
    );
}

#[tokio::test]
async fn test_handle_pr_triage_list_files_failure_reports_nothing() {
    let mut gh = MockGH::new();
    gh.expect_list_pr_files()
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(GITHUB_ERROR)))));
    gh.expect_create_check_run().never();

    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let triage_ctx = setup_triage_ctx(gh, pr_tx);
    triage_ctx.handle_pr_triage(setup_triage_input(PullRequestEventAction::Opened)).await;

    assert_eq!(pr_rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn test_handle_pr_triage_opened_rate_limit_enqueues_validate_defensively() {
    // Setup GitHub failing list_pr_files with a typed RateLimitError
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().times(1).returning(|_, _| {
        Box::pin(future::ready(Err(Error::new(RateLimitError::secondary(
            Some(Duration::from_secs(60)),
            "secondary limit",
        )))))
    });
    gh.expect_create_check_run().never();

    // Run triage for an opened PR
    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    setup_triage_ctx(gh, pr_tx)
        .handle_pr_triage(setup_triage_input(PullRequestEventAction::Opened))
        .await;

    // Check validation was enqueued defensively so its retry logic can recover
    assert_eq!(pr_rx.try_recv(), Ok(Job::Validate(setup_validate_input())));
}

#[tokio::test]
async fn test_handle_pr_triage_skips_superseded_commit_without_posting_check() {
    // Setup latest PR head pointing to a newer commit than HEAD_SHA
    let latest_pr_heads = LatestPrHeads::new();
    latest_pr_heads.record(ORG, PR_NUMBER, "newer-sha".to_string(), None, None);

    // Neither file listing nor check creation should run for the stale SHA
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().never();
    gh.expect_create_check_run().never();

    // Run triage for the superseded SHA
    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let mut triage_ctx = setup_triage_ctx(gh, pr_tx);
    triage_ctx.checks.latest_pr_heads = latest_pr_heads;
    triage_ctx.handle_pr_triage(setup_triage_input(PullRequestEventAction::Opened)).await;

    // Check nothing was enqueued
    assert_eq!(pr_rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn test_handle_pr_triage_truncated_file_list_enqueues_validate_instead_of_green_check() {
    // Setup truncated PR file list that did not include config.yaml in first 3,000 files
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().times(1).returning(|_, _| {
        Box::pin(future::ready(Ok(PrFiles {
            files: vec!["docs/README.md".to_string()],
            is_truncated: true,
        })))
    });
    gh.expect_create_check_run()
        .withf(|ctx, body| is_org_ctx(ctx) && is_in_progress_check(body))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    setup_triage_ctx(gh, pr_tx)
        .handle_pr_triage(setup_triage_input(PullRequestEventAction::Opened))
        .await;

    assert_eq!(pr_rx.try_recv(), Ok(Job::Validate(setup_validate_input())));
}

#[tokio::test]
async fn test_handle_pr_triage_unchanged_opened_reports_no_config_changes_check() {
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().times(1).returning(|_, _| {
        Box::pin(future::ready(Ok(PrFiles {
            files: vec!["docs/README.md".to_string()],
            is_truncated: false,
        })))
    });
    gh.expect_create_check_run()
        .withf(|ctx, body| {
            is_org_ctx(ctx)
                && is_pr_head_check(
                    body,
                    &ChecksCreateRequestConclusion::Success,
                    NO_CONFIG_CHANGES_MSG,
                )
        })
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let triage_ctx = setup_triage_ctx(gh, pr_tx);
    triage_ctx.handle_pr_triage(setup_triage_input(PullRequestEventAction::Opened)).await;

    assert_eq!(pr_rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn test_handle_pr_triage_validates_edited_and_reopened_pull_requests() {
    for action in [PullRequestEventAction::Edited, PullRequestEventAction::Reopened] {
        // Config changed -> creates in-progress check run and enqueues Validate
        let mut gh = MockGH::new();
        gh.expect_list_pr_files().times(1).returning(|_, _| {
            Box::pin(future::ready(Ok(PrFiles {
                files: vec![CONFIG_PATH.to_string()],
                is_truncated: false,
            })))
        });
        gh.expect_create_check_run()
            .withf(|ctx, body| is_org_ctx(ctx) && is_in_progress_check(body))
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(()))));

        let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
        setup_triage_ctx(gh, pr_tx).handle_pr_triage(setup_triage_input(action.clone())).await;
        assert_eq!(pr_rx.try_recv(), Ok(Job::Validate(setup_validate_input())));

        // Config unchanged -> creates passing no-config-changes check run
        let mut gh_unchanged = MockGH::new();
        gh_unchanged.expect_list_pr_files().times(1).returning(|_, _| {
            Box::pin(future::ready(Ok(PrFiles {
                files: vec!["README.md".to_string()],
                is_truncated: false,
            })))
        });
        gh_unchanged
            .expect_create_check_run()
            .withf(|ctx, body| {
                is_org_ctx(ctx)
                    && is_pr_head_check(
                        body,
                        &ChecksCreateRequestConclusion::Success,
                        NO_CONFIG_CHANGES_MSG,
                    )
            })
            .times(1)
            .returning(|_, _| Box::pin(future::ready(Ok(()))));

        let (pr_tx_unchanged, mut pr_rx_unchanged) = mpsc::unbounded_channel();
        setup_triage_ctx(gh_unchanged, pr_tx_unchanged)
            .handle_pr_triage(setup_triage_input(action))
            .await;
        assert_eq!(pr_rx_unchanged.try_recv(), Err(TryRecvError::Disconnected));
    }
}

#[tokio::test]
async fn test_handle_reconcile_job_comment_failure_is_best_effort() {
    // Setup service expectations
    let mut svc = MockService::new();
    svc.expect_reconcile().times(1).returning(|_, _| ReconcileOutcome::ok(vec![]));

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
    let result = worker
        .handle_reconcile_job(setup_reconcile_input(Some(PR_NUMBER)), CancellationToken::new())
        .await;

    // Check the job succeeds
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_reconcile_job_db_failure_is_best_effort() {
    // Setup service expectations
    let mut svc = MockService::new();
    svc.expect_reconcile().times(1).returning(|_, _| ReconcileOutcome::ok(vec![]));

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
    let result = worker
        .handle_reconcile_job(setup_reconcile_input(Some(PR_NUMBER)), CancellationToken::new())
        .await;

    // Check the job succeeds
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_reconcile_job_partial_failure_reaches_audit_and_comment() {
    // Setup services expectations (including a single service stopping early
    // with BOTH applied changes and a top-level error)
    let mut github_svc = MockService::new();
    github_svc
        .expect_reconcile()
        .withf(|org, _| is_org(org))
        .times(1)
        .returning(|_, _| ReconcileOutcome {
            changes_applied: vec![setup_change_applied()],
            error: Some(Error::new(RateLimitError::secondary(
                Some(Duration::from_secs(60)),
                "secondary limit",
            ))),
        });
    let mut other_svc = MockService::new();
    other_svc
        .expect_reconcile()
        .withf(|org, _| is_org(org))
        .times(1)
        .returning(|_, _| ReconcileOutcome::failed(format_err!(SERVICE_ERROR)));

    // Setup audit and comment expectations (each service rendered only once)
    let mut seq = Sequence::new();
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .withf(|input, changes_applied, errors| {
            *input == setup_reconcile_input(Some(PR_NUMBER))
                && changes_applied.len() == 1
                && changes_applied.get(GITHUB).is_some_and(|changes| changes.len() == 1)
                && errors.len() == 2
                && errors.contains_key(GITHUB)
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
                && body.matches("secondary limit").count() == 1
                && body.matches("team1").count() == 1
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
    let result = worker
        .handle_reconcile_job(setup_reconcile_input(Some(PR_NUMBER)), CancellationToken::new())
        .await;

    // Check the job ran to completion and reported not-clean
    assert!(!result.unwrap());
}

#[tokio::test]
async fn test_handle_reconcile_job_periodic_records_changes_without_commenting() {
    // Setup service expectations
    let mut svc = MockService::new();
    svc.expect_reconcile()
        .withf(|org, _| is_org(org))
        .times(1)
        .returning(|_, _| ReconcileOutcome::ok(vec![setup_change_applied()]));

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
    let result = worker.handle_reconcile_job(setup_reconcile_input(None), CancellationToken::new()).await;

    // Check the job converged cleanly
    assert!(result.unwrap());
}

#[tokio::test]
async fn test_handle_reconcile_job_periodic_records_service_error_without_commenting() {
    // Setup service expectations
    let mut svc = MockService::new();
    svc.expect_reconcile()
        .withf(|org, _| is_org(org))
        .times(1)
        .returning(|_, _| ReconcileOutcome::failed(format_err!(SERVICE_ERROR)));

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
    let result = worker.handle_reconcile_job(setup_reconcile_input(None), CancellationToken::new()).await;

    // Check the job recorded the error and reported not-clean
    assert!(!result.unwrap());
}

#[tokio::test]
async fn test_handle_reconcile_job_posts_partial_comment_over_real_pool_on_cancel_jobs() {
    // Setup a real FakeGitHub + ClientPool with transport_cancel kept open
    // during cleanup grace after cancel_jobs interrupts PR reconciliation
    let comments_path = format!("/repos/{ORG}/{REPO}/issues/{PR_NUMBER}/comments");
    let fake = clowarden_core::github::http_fake::FakeGitHub::start(vec![
        clowarden_core::github::http_fake::Route::new(
            "POST",
            &comments_path,
            201,
            r#"{"id":1,"author_association":"NONE"}"#,
        ),
    ]);
    let policy = clowarden_core::github::RateLimitPolicy {
        jitter_max: Duration::ZERO,
        mutation_interval: Duration::ZERO,
        ..clowarden_core::github::RateLimitPolicy::default()
    };
    let cancel_jobs = CancellationToken::new();
    let transport_cancel = CancellationToken::new();
    let pool = fake.client_pool_with_shutdown(policy, transport_cancel.clone());
    let gh: DynGH = Arc::new(github::GHApi::new(pool));

    // Service applies one change, waits for job_cancel, and returns partial outcome
    let started = Arc::new(tokio::sync::Notify::new());
    let svc = Arc::new(CancelAwareService {
        started: started.clone(),
    });
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Run PR reconcile inside OrgWorker so job_cancel derives from cancel_jobs
    let (pr_tx, pr_rx) = mpsc::unbounded_channel();
    let (_periodic_tx, periodic_rx) = mpsc::channel(1);
    let stop_worker = CancellationToken::new();
    let worker = OrgWorker::new(JobsContext {
        cancel_jobs: cancel_jobs.clone(),
        clean_tracker: CleanReconcileTracker::new(),
        db: Arc::new(db),
        deferred_retries: DeferredRetries::new(),
        gh,
        ghc: Arc::new(FakeCoreGH::default()),
        latest_pr_heads: LatestPrHeads::new(),
        pr_jobs_tx: pr_tx.clone(),
        reconcile_frequency: ONE_HOUR,
        services: HashMap::from([(GITHUB, svc as DynServiceHandler)]),
        stop_intake: CancellationToken::new(),
        stop_workers: stop_worker.clone(),
        transport_cancel,
    });
    let worker_handle = worker.run(pr_rx, periodic_rx, stop_worker.clone(), OrgWorkerActivity::new());

    pr_tx.send(Job::Reconcile(setup_reconcile_input(Some(PR_NUMBER)))).unwrap();
    started.notified().await;

    // Simulate drain deadline expiring: cancel_jobs fires while transport stays open
    cancel_jobs.cancel();
    stop_worker.cancel();
    timeout(Duration::from_secs(2), worker_handle)
        .await
        .expect("worker should post partial comment and exit within grace period")
        .unwrap();

    // Check the partial reconciliation comment actually reached FakeGitHub over HTTP
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, comments_path);
    let body = requests[0].body.as_ref().unwrap()["body"].as_str().unwrap();
    assert!(body.contains("Some errors were found during the reconciliation"));
    assert!(body.contains("Changes applied successfully:"));
    assert!(body.contains("github api request cancelled"));
}

#[tokio::test]
async fn test_handle_reconcile_job_posts_pr_comment_while_db_register_is_pending() {
    // Setup a service returning a partial outcome and a slow database write
    let mut svc = MockService::new();
    svc.expect_reconcile().times(1).returning(|_, _| ReconcileOutcome {
        changes_applied: vec![setup_change_applied()],
        error: Some(Error::new(clowarden_core::github::CancelledError)),
    });
    let release_db = CancellationToken::new();
    let release_db_clone = release_db.clone();
    let mut db = MockDB::new();
    db.expect_register_reconciliation().times(1).returning(move |_, _, _| {
        let release = release_db_clone.clone();
        Box::pin(async move {
            release.cancelled().await;
            Ok(())
        })
    });
    let comment_posted = Arc::new(tokio::sync::Notify::new());
    let comment_posted_clone = comment_posted.clone();
    let mut gh = MockGH::new();
    gh.expect_post_comment().times(1).returning(move |_, _, _| {
        comment_posted_clone.notify_one();
        Box::pin(future::ready(Ok(1)))
    });

    // Run handle_reconcile_job and verify the PR comment posts before DB finishes
    let worker = setup_worker(db, gh, Arc::new(FakeCoreGH::default()), [(GITHUB, Arc::new(svc))]);
    let job = tokio::spawn(async move {
        worker
            .handle_reconcile_job(setup_reconcile_input(Some(PR_NUMBER)), CancellationToken::new())
            .await
    });

    timeout(Duration::from_secs(1), comment_posted.notified())
        .await
        .expect("PR comment should be posted concurrently without waiting for slow DB registration");
    release_db.cancel();
    assert!(!job.await.unwrap().unwrap());
}

#[tokio::test(start_paused = true)]
async fn test_handle_validate_job_defers_retry_on_rate_limit_and_fails_check_when_exhausted() {
    // First attempt hits rate limit -> defers a retry without posting a verdict
    let mut svc = MockService::new();
    svc.expect_get_changes_summary().times(1).returning(|_, _, _| {
        Err(Error::new(RateLimitError::secondary(
            Some(Duration::from_secs(5)),
            "secondary limit",
        )))
    });
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut gh = MockGH::new();
    gh.expect_post_comment().never();
    gh.expect_create_check_run().never();

    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let worker = setup_worker_with_pr_tx(
        MockDB::new(),
        gh,
        ghc.clone(),
        [(GITHUB, Arc::new(svc))],
        pr_tx.clone(),
    );
    let err = worker
        .handle_validate_job(setup_validate_input(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(is_rate_limit_error(&err));

    // Check deferred retry arrives after the cooldown with incremented attempt
    let retried = timeout(Duration::from_secs(10), pr_rx.recv()).await.unwrap().unwrap();
    assert_eq!(
        retried,
        Job::Validate(ValidateInput {
            retry_attempt: 1,
            ..setup_validate_input()
        })
    );

    // Final attempt with retries exhausted fails the check run (fail-closed)
    let mut svc2 = MockService::new();
    svc2.expect_get_changes_summary().times(1).returning(|_, _, _| {
        Err(Error::new(RateLimitError::secondary(
            Some(Duration::from_secs(5)),
            "secondary limit",
        )))
    });
    let mut gh2 = MockGH::new();
    gh2.expect_post_comment().never();
    gh2.expect_create_check_run()
        .withf(|ctx, body| {
            is_org_ctx(ctx)
                && is_pr_head_check(
                    body,
                    &ChecksCreateRequestConclusion::Failure,
                    RATE_LIMITED_VALIDATION_MSG,
                )
        })
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    let worker2 = setup_worker_with_pr_tx(MockDB::new(), gh2, ghc, [(GITHUB, Arc::new(svc2))], pr_tx);
    let exhausted_input = ValidateInput {
        retry_attempt: MAX_VALIDATE_RATE_LIMIT_RETRIES,
        ..setup_validate_input()
    };
    let err2 = worker2.handle_validate_job(exhausted_input, CancellationToken::new()).await.unwrap_err();
    assert!(is_rate_limit_error(&err2));
}

#[tokio::test(start_paused = true)]
async fn test_handle_validate_job_deferred_retry_closes_check_as_failure_on_shutdown() {
    // Setup validation hitting a rate limit with a 60s retry delay
    let mut svc = MockService::new();
    svc.expect_get_changes_summary().times(1).returning(|_, _, _| {
        Err(Error::new(RateLimitError::secondary(
            Some(Duration::from_secs(60)),
            "secondary limit",
        )))
    });
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);

    // Expect fail-closed check completion when stop_intake interrupts the retry sleep
    let mut gh = MockGH::new();
    gh.expect_post_comment().never();
    gh.expect_create_check_run()
        .withf(|ctx, body| {
            is_org_ctx(ctx)
                && is_pr_head_check(
                    body,
                    &ChecksCreateRequestConclusion::Failure,
                    INTERRUPTED_VALIDATION_MSG,
                )
        })
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Run validation to schedule the deferred retry
    let stop_intake = CancellationToken::new();
    let deferred_retries = DeferredRetries::new();
    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let worker = OrgWorker::new(JobsContext {
        cancel_jobs: CancellationToken::new(),
        clean_tracker: CleanReconcileTracker::new(),
        db: Arc::new(MockDB::new()),
        deferred_retries: deferred_retries.clone(),
        gh: Arc::new(gh),
        ghc,
        latest_pr_heads: LatestPrHeads::new(),
        pr_jobs_tx: pr_tx,
        reconcile_frequency: ONE_HOUR,
        services: HashMap::from([(GITHUB, Arc::new(svc) as DynServiceHandler)]),
        stop_intake: stop_intake.clone(),
        stop_workers: CancellationToken::new(),
        transport_cancel: CancellationToken::new(),
    });
    let err = worker
        .handle_validate_job(setup_validate_input(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(is_rate_limit_error(&err));

    // Trigger shutdown intake stop while the retry is sleeping and wait for it
    stop_intake.cancel();
    deferred_retries.join_all().await;

    // Check no retry was enqueued and the neutral check expectation was met
    assert_eq!(pr_rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn test_handle_validate_job_check_failure_is_returned() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));

    // Setup GitHub expectations
    let mut gh = MockGH::new();
    gh.expect_post_comment().times(1).returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run()
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Err(format_err!(GITHUB_ERROR)))));

    // Run the job
    let worker = setup_worker(MockDB::new(), gh, ghc, [(GITHUB, Arc::new(svc))]);
    let result = worker.handle_validate_job(setup_validate_input(), CancellationToken::new()).await;

    // Check the job fails with the GitHub error
    assert_eq!(result.unwrap_err().to_string(), GITHUB_ERROR);
}

#[tokio::test]
async fn test_handle_validate_job_comment_failure_is_noted_in_failed_check() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _, _| Err(format_err!(SERVICE_ERROR)));

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
    let result = worker.handle_validate_job(setup_validate_input(), CancellationToken::new()).await;

    // Check the job fails with the validation error, not with the GitHub one
    let err = result.unwrap_err().to_string();
    assert!(err.contains(SERVICE_ERROR), "unexpected error: {err}");
    assert!(!err.contains(GITHUB_ERROR), "unexpected error: {err}");
}

#[tokio::test]
async fn test_handle_validate_job_comment_failure_is_noted_in_successful_check() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));

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
    let result = worker.handle_validate_job(setup_validate_input(), CancellationToken::new()).await;

    // Check the job succeeds, as the check run has been completed
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_validate_job_fork_head_is_read_from_fork_at_pinned_sha() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .withf(|org, head_src, _| {
            is_org(org)
                && head_src.inst_id == Some(INSTALLATION_ID)
                && head_src.owner == FORK_OWNER
                && head_src.repo == FORK_REPO
                && head_src.ref_ == HEAD_SHA
        })
        .times(1)
        .returning(|_, _, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));

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
    let result = worker.handle_validate_job(input, CancellationToken::new()).await;

    // Check head config was read from the fork at the pinned SHA and base from the org
    assert!(result.is_ok());
    assert_fetches(
        &ghc,
        vec![
            Fetch::new(FORK_OWNER, FORK_REPO, HEAD_SHA),
            Fetch::new(ORG, REPO, BASE_REF),
        ],
    );
}

#[tokio::test]
async fn test_handle_validate_job_head_without_repo_is_read_from_org_at_pinned_sha() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .withf(|_, head_src, _| head_src.owner == ORG && head_src.repo == REPO && head_src.ref_ == HEAD_SHA)
        .times(1)
        .returning(|_, _, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));

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
    let result = worker.handle_validate_job(setup_validate_input(), CancellationToken::new()).await;

    // Check both configs were read from the org at exact pinned refs
    assert!(result.is_ok());
    assert_fetches(
        &ghc,
        vec![Fetch::new(ORG, REPO, HEAD_SHA), Fetch::new(ORG, REPO, BASE_REF)],
    );
}

#[tokio::test]
async fn test_handle_validate_job_posts_fail_closed_check_over_real_pool_on_cancel_jobs() {
    // Setup a real FakeGitHub + ClientPool with transport_cancel kept open
    // during cleanup grace after cancel_jobs interrupts validation
    let check_runs_path = format!("/repos/{ORG}/{REPO}/check-runs");
    let fake = clowarden_core::github::http_fake::FakeGitHub::start(vec![
        clowarden_core::github::http_fake::Route::new("POST", &check_runs_path, 201, r#"{"output": {}}"#),
    ]);
    let policy = clowarden_core::github::RateLimitPolicy {
        jitter_max: Duration::ZERO,
        mutation_interval: Duration::ZERO,
        ..clowarden_core::github::RateLimitPolicy::default()
    };
    let cancel_jobs = CancellationToken::new();
    let transport_cancel = CancellationToken::new();
    let pool = fake.client_pool_with_shutdown(policy, transport_cancel.clone());
    let gh: DynGH = Arc::new(github::GHApi::new(pool));

    // Service blocks until job_cancel fires, then returns CancelledError
    let started = Arc::new(tokio::sync::Notify::new());
    let svc = Arc::new(CancelAwareService {
        started: started.clone(),
    });

    // Run PR validation inside OrgWorker so job_cancel derives from cancel_jobs
    let (pr_tx, pr_rx) = mpsc::unbounded_channel();
    let (_periodic_tx, periodic_rx) = mpsc::channel(1);
    let stop_worker = CancellationToken::new();
    let worker = OrgWorker::new(JobsContext {
        cancel_jobs: cancel_jobs.clone(),
        clean_tracker: CleanReconcileTracker::new(),
        db: Arc::new(MockDB::new()),
        deferred_retries: DeferredRetries::new(),
        gh,
        ghc: FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]),
        latest_pr_heads: LatestPrHeads::new(),
        pr_jobs_tx: pr_tx.clone(),
        reconcile_frequency: ONE_HOUR,
        services: HashMap::from([(GITHUB, svc as DynServiceHandler)]),
        stop_intake: CancellationToken::new(),
        stop_workers: stop_worker.clone(),
        transport_cancel,
    });
    let worker_handle = worker.run(pr_rx, periodic_rx, stop_worker.clone(), OrgWorkerActivity::new());

    pr_tx.send(Job::Validate(setup_validate_input())).unwrap();
    started.notified().await;

    // Simulate drain deadline expiring: cancel_jobs fires while transport stays open
    cancel_jobs.cancel();
    stop_worker.cancel();
    timeout(Duration::from_secs(2), worker_handle)
        .await
        .expect("worker should finish cleanup within grace period")
        .unwrap();

    // Check the fail-closed check run actually reached FakeGitHub over HTTP
    let requests = fake.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, check_runs_path);
    let body = requests[0].body.as_ref().unwrap();
    assert_eq!(body["status"], "completed");
    assert_eq!(body["conclusion"], "failure");
    assert_eq!(body["output"]["summary"], INTERRUPTED_VALIDATION_MSG);
}

#[tokio::test]
async fn test_handle_validate_job_service_error_fails_check() {
    // Setup configuration and services expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut github_svc = MockService::new();
    github_svc
        .expect_get_changes_summary()
        .times(1)
        .returning(|_, _, _| Err(format_err!(SERVICE_ERROR)));
    let mut other_svc = MockService::new();
    other_svc
        .expect_get_changes_summary()
        .times(1)
        .returning(|_, _, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));

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
    let result = worker.handle_validate_job(setup_validate_input(), CancellationToken::new()).await;

    // Check the job fails with the validation error
    let err = result.unwrap_err().to_string();
    assert!(err.contains(SERVICE_ERROR), "unexpected error: {err}");
}

#[tokio::test]
async fn test_handle_validate_job_services_errors_are_aggregated() {
    // Setup configuration and services expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut github_svc = MockService::new();
    github_svc
        .expect_get_changes_summary()
        .times(1)
        .returning(|_, _, _| Err(format_err!("first service error")));
    let mut other_svc = MockService::new();
    other_svc
        .expect_get_changes_summary()
        .times(1)
        .returning(|_, _, _| Err(format_err!("second service error")));

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
    let result = worker.handle_validate_job(setup_validate_input(), CancellationToken::new()).await;

    // Check the job fails with both errors
    let err = result.unwrap_err().to_string();
    assert!(err.contains("first service error"), "unexpected error: {err}");
    assert!(err.contains("second service error"), "unexpected error: {err}");
}

#[tokio::test]
async fn test_handle_validate_job_success_reports_on_pr_head() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary().times(1).returning(|_, _, _| {
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
    let result = worker.handle_validate_job(setup_validate_input(), CancellationToken::new()).await;

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
    let ghc = FakeCoreGH::new([(BASE_REF, Err("file not found")), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Invalid)));

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
    let result = worker.handle_validate_job(setup_validate_input(), CancellationToken::new()).await;

    // Check the job succeeds, so that a broken base config can be repaired
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_validate_job_validation_error_outlives_github_failures() {
    // Setup configuration and service expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _, _| Err(format_err!(SERVICE_ERROR)));

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
    let result = worker.handle_validate_job(setup_validate_input(), CancellationToken::new()).await;

    // Check the job fails with the validation error, not with the GitHub one
    let err = result.unwrap_err().to_string();
    assert!(err.contains(SERVICE_ERROR), "unexpected error: {err}");
    assert!(!err.contains(GITHUB_ERROR), "unexpected error: {err}");
}

#[tokio::test(start_paused = true)]
async fn test_handler_runs_pr_job_on_other_org_while_periodic_runs_and_serializes_within_org() {
    // Setup a service that blocks each job until it is released
    let (svc, mut started_rx) = GatedService::new([ORG, OTHER_ORG]);
    let services = HashMap::from([(GITHUB, svc.clone() as DynServiceHandler)]);

    // Setup audit expectations
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .times(3)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Start the handler
    let (pr_jobs_tx, pr_jobs_rx) = mpsc::unbounded_channel();
    let (periodic_tx, periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    let stop_workers = CancellationToken::new();
    let transport_cancel = CancellationToken::new();
    let ctx = JobsContext {
        cancel_jobs: CancellationToken::new(),
        clean_tracker: CleanReconcileTracker::new(),
        db: Arc::new(db),
        deferred_retries: DeferredRetries::new(),
        gh: Arc::new(MockGH::new()),
        ghc: Arc::new(FakeCoreGH::default()),
        latest_pr_heads: LatestPrHeads::new(),
        pr_jobs_tx: pr_jobs_tx.clone(),
        reconcile_frequency: ONE_HOUR,
        services,
        stop_intake: stop_intake.clone(),
        stop_workers: stop_workers.clone(),
        transport_cancel: transport_cancel.clone(),
    };
    let workers = handler(
        &ctx,
        pr_jobs_rx,
        periodic_rx,
        &[setup_org(ORG), setup_org(OTHER_ORG)],
    );

    // Start a periodic job for ORG, then send PR jobs for unknown-org, ORG, and OTHER_ORG
    let (done_tx, done_rx) = oneshot::channel();
    periodic_tx
        .send(QueuedJob {
            job: setup_periodic_job(setup_org(ORG)),
            done: Some(done_tx),
        })
        .await
        .unwrap();
    assert_eq!(next_started(&mut started_rx).await, ORG);

    for org_name in ["unknown-org", ORG, OTHER_ORG] {
        pr_jobs_tx.send(setup_periodic_job(setup_org(org_name))).unwrap();
    }

    // OTHER_ORG's PR job starts immediately while ORG's periodic job is still running
    assert_eq!(next_started(&mut started_rx).await, OTHER_ORG);
    assert_nothing_else_starts(&mut started_rx).await;

    // Releasing ORG's periodic job notifies done_rx and starts ORG's queued PR job
    svc.release(ORG);
    assert_eq!(done_rx.await, Ok(PeriodicJobOutcome::Clean));
    assert_eq!(next_started(&mut started_rx).await, ORG);
    assert_nothing_else_starts(&mut started_rx).await;

    // Finish the remaining jobs and stop the handler
    svc.release(ORG);
    svc.release(OTHER_ORG);
    stop_intake.cancel();
    stop_workers.cancel();
    assert!(future::join_all(workers).await.into_iter().all(|result| result.is_ok()));

    // Check the job for the unknown organization did not run
    assert_eq!(started_rx.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test(start_paused = true)]
async fn test_handler_defers_periodic_job_immediately_when_org_worker_is_busy_with_pr() {
    // Setup a gated service blocking ORG inside a PR job
    let (svc, mut started_rx) = GatedService::new([ORG]);
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Start the handler
    let (pr_jobs_tx, pr_jobs_rx) = mpsc::unbounded_channel();
    let (periodic_tx, periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    let stop_workers = CancellationToken::new();
    let ctx = JobsContext {
        cancel_jobs: CancellationToken::new(),
        clean_tracker: CleanReconcileTracker::new(),
        db: Arc::new(db),
        deferred_retries: DeferredRetries::new(),
        gh: Arc::new(MockGH::new()),
        ghc: Arc::new(FakeCoreGH::default()),
        latest_pr_heads: LatestPrHeads::new(),
        pr_jobs_tx: pr_jobs_tx.clone(),
        reconcile_frequency: ONE_HOUR,
        services: HashMap::from([(GITHUB, svc.clone() as DynServiceHandler)]),
        stop_intake: stop_intake.clone(),
        stop_workers: stop_workers.clone(),
        transport_cancel: CancellationToken::new(),
    };
    let workers = handler(&ctx, pr_jobs_rx, periodic_rx, &[setup_org(ORG)]);

    // Start a PR job on ORG and wait until it is executing
    pr_jobs_tx.send(setup_periodic_job(setup_org(ORG))).unwrap();
    assert_eq!(next_started(&mut started_rx).await, ORG);

    // Send a periodic job for ORG while it is busy; router must defer immediately
    let (done_tx, done_rx) = oneshot::channel();
    periodic_tx
        .send(QueuedJob {
            job: setup_periodic_job(setup_org(ORG)),
            done: Some(done_tx),
        })
        .await
        .unwrap();
    let outcome = timeout(Duration::from_secs(1), done_rx)
        .await
        .expect("periodic job should be deferred immediately without blocking")
        .unwrap();
    assert_eq!(outcome, PeriodicJobOutcome::DeferredBusy);

    // Finish the PR job and stop the handler
    svc.release(ORG);
    stop_intake.cancel();
    stop_workers.cancel();
    assert!(future::join_all(workers).await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn test_handler_drains_pr_jobs_arriving_after_stop_intake_until_stop_workers() {
    // Setup service and audit expectations for a PR job arriving during triage drain
    let mut svc = MockService::new();
    svc.expect_reconcile().times(1).returning(|_, _| ReconcileOutcome::ok(vec![]));
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .withf(|input, _, _| input.pr_number == Some(PR_NUMBER))
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));
    let mut gh = MockGH::new();
    gh.expect_post_comment().times(1).returning(|_, _, _| Box::pin(future::ready(Ok(1))));

    // Start the handler, then cancel stop_intake (simulating triage still draining)
    let (pr_jobs_tx, pr_jobs_rx) = mpsc::unbounded_channel();
    let (_periodic_tx, periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    let stop_workers = CancellationToken::new();
    let ctx = JobsContext {
        cancel_jobs: CancellationToken::new(),
        clean_tracker: CleanReconcileTracker::new(),
        db: Arc::new(db),
        deferred_retries: DeferredRetries::new(),
        gh: Arc::new(gh),
        ghc: Arc::new(FakeCoreGH::default()),
        latest_pr_heads: LatestPrHeads::new(),
        pr_jobs_tx: pr_jobs_tx.clone(),
        reconcile_frequency: ONE_HOUR,
        services: HashMap::from([(GITHUB, Arc::new(svc) as DynServiceHandler)]),
        stop_intake: stop_intake.clone(),
        stop_workers: stop_workers.clone(),
        transport_cancel: CancellationToken::new(),
    };
    let workers = handler(&ctx, pr_jobs_rx, periodic_rx, &[setup_org(ORG)]);

    // Cancel intake first, then let a late triage task enqueue a PR job
    stop_intake.cancel();
    tokio::task::yield_now().await;
    pr_jobs_tx.send(Job::Reconcile(setup_reconcile_input(Some(PR_NUMBER)))).unwrap();

    // Signal workers to drain and exit once triage is done
    stop_workers.cancel();
    assert!(future::join_all(workers).await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn test_org_worker_run_finishes_queued_pr_jobs_after_cancellation() {
    // Setup service expectations
    let mut svc = MockService::new();
    svc.expect_reconcile().times(2).returning(|_, _| ReconcileOutcome::ok(vec![]));

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

    // Queue the PR jobs and cancel stop_worker before starting (transport remains live)
    let (pr_tx, pr_rx) = mpsc::unbounded_channel();
    let (_periodic_tx, periodic_rx) = mpsc::channel(1);
    for pr_number in [1, 2] {
        pr_tx.send(Job::Reconcile(setup_reconcile_input(Some(pr_number)))).unwrap();
    }
    let stop_intake = CancellationToken::new();
    stop_intake.cancel();
    let stop_worker = CancellationToken::new();
    stop_worker.cancel();

    // Run the worker
    let worker = setup_worker_with_tokens(
        db,
        gh,
        Arc::new(FakeCoreGH::default()),
        [(GITHUB, Arc::new(svc))],
        stop_intake,
        CancellationToken::new(),
    );
    let result = worker.run(pr_rx, periodic_rx, stop_worker, OrgWorkerActivity::new()).await;

    // Check the worker exits after completing the queued PR jobs
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
    let (pr_tx, pr_rx) = mpsc::unbounded_channel();
    let (_periodic_tx, periodic_rx) = mpsc::channel(1);
    pr_tx.send(setup_periodic_job(setup_org(ORG))).unwrap();
    let stop_intake = CancellationToken::new();
    let stop_worker = CancellationToken::new();
    let worker = setup_worker_with_tokens(
        db,
        MockGH::new(),
        Arc::new(FakeCoreGH::default()),
        [(GITHUB, svc.clone())],
        stop_intake.clone(),
        CancellationToken::new(),
    );
    let mut worker_done = worker.run(pr_rx, periodic_rx, stop_worker.clone(), OrgWorkerActivity::new());
    assert_eq!(next_started(&mut started_rx).await, ORG);

    // Check the worker keeps waiting for the job after cancellation
    stop_intake.cancel();
    stop_worker.cancel();
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
async fn test_scheduler_applies_backoff_to_persistently_failing_org() {
    // Setup DB showing ORG failed 5 minutes ago (within 15-minute base backoff)
    let now = OffsetDateTime::now_utc();
    let recent_fail = now - ::time::Duration::minutes(5);
    let mut db = MockDB::new();
    db.expect_get_reconciliation_schedule_state().times(1).returning(move || {
        let map = HashMap::from([(
            ORG.to_string(),
            OrgReconciliationStatus {
                last_attempt_at: Some(recent_fail),
                last_clean_at: None,
            },
        )]);
        Box::pin(future::ready(Ok(map)))
    });

    // Start the scheduler
    let (periodic_tx, mut periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    let scheduler_done = scheduler(
        Arc::new(db),
        CleanReconcileTracker::new(),
        periodic_tx,
        stop_intake.clone(),
        &[setup_org(ORG)],
        ONE_HOUR,
    );

    // Check ORG is NOT re-dispatched after 2 minutes (no 60-second failure loop)
    let premature = timeout(Duration::from_secs(120), periodic_rx.recv()).await;
    assert!(premature.is_err());

    // Stop scheduler cleanly
    stop_intake.cancel();
    assert!(future::join_all(scheduler_done).await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_does_not_waste_slot_spacing_on_deferred_busy_org() {
    // Setup two due orgs with 30-minute slot_spacing
    let mut db = MockDB::new();
    db.expect_get_reconciliation_schedule_state()
        .times(1)
        .returning(|| Box::pin(future::ready(Ok(HashMap::new()))));

    // Start scheduler
    let orgs = [setup_org("alpha"), setup_org("beta")];
    let (periodic_tx, mut periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    let start = Instant::now();
    let scheduler_done = scheduler(
        Arc::new(db),
        CleanReconcileTracker::new(),
        periodic_tx,
        stop_intake.clone(),
        &orgs,
        ONE_HOUR,
    );

    // First org ("alpha") returns DeferredBusy
    let first = periodic_rx.recv().await.unwrap();
    assert_eq!(first.job.org_name(), "alpha");
    first.done.unwrap().send(PeriodicJobOutcome::DeferredBusy).unwrap();

    // Second org ("beta") must start immediately without waiting 30 minutes
    let second = timeout(Duration::from_secs(1), periodic_rx.recv())
        .await
        .expect("beta should dispatch immediately after alpha deferred")
        .unwrap();
    assert_eq!(second.job.org_name(), "beta");
    assert_eq!(start.elapsed(), Duration::ZERO);
    second.done.unwrap().send(PeriodicJobOutcome::Clean).unwrap();

    // Stop scheduler cleanly
    stop_intake.cancel();
    assert!(future::join_all(scheduler_done).await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_exits_promptly_when_cancelled_during_blocked_db_query() {
    // Setup DB whose schedule query hangs forever
    let mut db = MockDB::new();
    db.expect_get_reconciliation_schedule_state()
        .times(1)
        .returning(|| Box::pin(future::pending()));

    // Start the scheduler and cancel it while blocked on the DB query
    let (periodic_tx, _periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    let scheduler_done = scheduler(
        Arc::new(db),
        CleanReconcileTracker::new(),
        periodic_tx,
        stop_intake.clone(),
        &[setup_org(ORG)],
        ONE_HOUR,
    );
    tokio::task::yield_now().await;
    stop_intake.cancel();

    // Check scheduler exits immediately without waiting for DB timeout
    let results = timeout(Duration::from_secs(1), future::join_all(scheduler_done))
        .await
        .expect("scheduler should exit promptly on cancel");
    assert!(results.into_iter().all(|result| result.is_ok()));
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_handles_empty_org_list() {
    // Setup DB that must never be queried when no orgs exist
    let mut db = MockDB::new();
    db.expect_get_reconciliation_schedule_state().never();
    let (periodic_tx, mut periodic_rx) = mpsc::channel(1);

    // Run scheduler with empty org list
    let results = future::join_all(scheduler(
        Arc::new(db),
        CleanReconcileTracker::new(),
        periodic_tx,
        CancellationToken::new(),
        &[],
        ONE_HOUR,
    ))
    .await;

    // Check scheduler exited cleanly without dispatching anything
    assert!(results.into_iter().all(|result| result.is_ok()));
    assert!(periodic_rx.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_orders_oldest_first_skips_recent_and_runs_sequentially() {
    // Setup DB where OTHER_ORG was cleanly reconciled 10 minutes ago (fresh),
    // ORG was reconciled 2 hours ago (due), and "never-org" has never had a
    // clean reconciliation (due first)
    let now = OffsetDateTime::now_utc();
    let fresh_ts = now - ::time::Duration::minutes(10);
    let stale_ts = now - ::time::Duration::hours(2);
    let mut db = MockDB::new();
    db.expect_get_reconciliation_schedule_state().times(1).returning(move || {
        let map = HashMap::from([
            (
                OTHER_ORG.to_string(),
                OrgReconciliationStatus {
                    last_attempt_at: Some(fresh_ts),
                    last_clean_at: Some(fresh_ts),
                },
            ),
            (
                ORG.to_string(),
                OrgReconciliationStatus {
                    last_attempt_at: Some(stale_ts),
                    last_clean_at: Some(stale_ts),
                },
            ),
        ]);
        Box::pin(future::ready(Ok(map)))
    });

    // Start the scheduler
    let orgs = [setup_org(OTHER_ORG), setup_org(ORG), setup_org("never-org")];
    let (periodic_tx, mut periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    let scheduler_done = scheduler(
        Arc::new(db),
        CleanReconcileTracker::new(),
        periodic_tx,
        stop_intake.clone(),
        &orgs,
        ONE_HOUR,
    );

    // First dispatched must be "never-org" (never cleanly reconciled)
    let first = periodic_rx.recv().await.unwrap();
    assert_eq!(first.job.org_name(), "never-org");

    // Second must NOT be dispatched until first signals completion
    let premature = timeout(Duration::from_secs(1), periodic_rx.recv()).await;
    assert!(premature.is_err());
    first.done.unwrap().send(PeriodicJobOutcome::Clean).unwrap();

    // Second dispatched must be ORG (stale), paced by slot_spacing (20 min)
    let second = periodic_rx.recv().await.unwrap();
    assert_eq!(second.job.org_name(), ORG);
    second.done.unwrap().send(PeriodicJobOutcome::Clean).unwrap();

    // Stop scheduler cleanly
    stop_intake.cancel();
    assert!(future::join_all(scheduler_done).await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_retries_org_when_newer_attempt_failed_after_recent_clean_run() {
    // Setup DB where ORG ran cleanly 2h ago (inside 6h frequency) but a merged
    // PR reconcile failed 20m ago (past 15m base failure backoff)
    let now = OffsetDateTime::now_utc();
    let recent_clean = now - ::time::Duration::hours(2);
    let newer_failure = now - ::time::Duration::minutes(20);
    let mut db = MockDB::new();
    db.expect_get_reconciliation_schedule_state().times(1).returning(move || {
        let map = HashMap::from([(
            ORG.to_string(),
            OrgReconciliationStatus {
                last_attempt_at: Some(newer_failure),
                last_clean_at: Some(recent_clean),
            },
        )]);
        Box::pin(future::ready(Ok(map)))
    });

    // Start scheduler with 6-hour frequency
    let (periodic_tx, mut periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    let scheduler_done = scheduler(
        Arc::new(db),
        CleanReconcileTracker::new(),
        periodic_tx,
        stop_intake.clone(),
        &[setup_org(ORG)],
        Duration::from_hours(6),
    );

    // ORG must be dispatched immediately instead of being hidden by recent_clean
    let job = timeout(Duration::from_secs(1), periodic_rx.recv())
        .await
        .expect("org with newer failure should be retried")
        .unwrap();
    assert_eq!(job.job.org_name(), ORG);
    job.done.unwrap().send(PeriodicJobOutcome::Clean).unwrap();

    // Stop scheduler cleanly
    stop_intake.cancel();
    assert!(future::join_all(scheduler_done).await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_sends_no_jobs_when_already_cancelled() {
    // Setup a token that is already cancelled
    let mut db = MockDB::new();
    db.expect_get_reconciliation_schedule_state().never();
    let (periodic_tx, mut periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    stop_intake.cancel();

    // Run the scheduler
    let results = future::join_all(scheduler(
        Arc::new(db),
        CleanReconcileTracker::new(),
        periodic_tx,
        stop_intake,
        &[setup_org(ORG)],
        ONE_HOUR,
    ))
    .await;

    // Check it exited without sending any job
    assert!(results.into_iter().all(|result| result.is_ok()));
    assert!(periodic_rx.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_skips_org_reconciled_cleanly_during_slot_wait() {
    // Setup two due orgs (slot_spacing = 30 min)
    let mut db = MockDB::new();
    db.expect_get_reconciliation_schedule_state()
        .times(1)
        .returning(|| Box::pin(future::ready(Ok(HashMap::new()))));

    // Start scheduler sharing clean_tracker
    let clean_tracker = CleanReconcileTracker::new();
    let orgs = [setup_org("alpha"), setup_org("beta")];
    let (periodic_tx, mut periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    let scheduler_done = scheduler(
        Arc::new(db),
        clean_tracker.clone(),
        periodic_tx,
        stop_intake.clone(),
        &orgs,
        ONE_HOUR,
    );

    // "alpha" dispatches first; while waiting for "beta"'s slot, a clean PR
    // reconcile completes for "beta"
    let first = periodic_rx.recv().await.unwrap();
    assert_eq!(first.job.org_name(), "alpha");
    clean_tracker.record_outcome("beta", OffsetDateTime::now_utc(), true);
    first.done.unwrap().send(PeriodicJobOutcome::Clean).unwrap();

    // "beta" must be skipped when its slot arrives (30 min later)
    let skipped = timeout(Duration::from_mins(35), periodic_rx.recv()).await;
    assert!(skipped.is_err());

    // Stop scheduler cleanly
    stop_intake.cancel();
    assert!(future::join_all(scheduler_done).await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn test_handle_check_run_rerequested_retriggers_validation_for_embedded_pr() {
    let mut gh = MockGH::new();
    gh.expect_get_pr()
        .withf(|ctx, pr_number| is_org_ctx(ctx) && *pr_number == PR_NUMBER)
        .times(1)
        .returning(|_, _| {
            Box::pin(future::ready(Ok(AssociatedPullRequest {
                base_ref: BASE_REF.to_string(),
                head_ref: HEAD_REF.to_string(),
                head_sha: HEAD_SHA.to_string(),
                is_open: true,
                number: PR_NUMBER,
                head_owner: Some(FORK_OWNER.to_string()),
                head_repo: Some(FORK_REPO.to_string()),
                updated_at: None,
            })))
        });
    gh.expect_list_pr_files().times(1).returning(|_, _| {
        Box::pin(future::ready(Ok(PrFiles {
            files: vec![CONFIG_PATH.to_string()],
            is_truncated: false,
        })))
    });
    gh.expect_create_check_run()
        .withf(|ctx, body| is_org_ctx(ctx) && is_in_progress_check(body))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    setup_triage_ctx(gh, pr_tx)
        .handle_check_run_rerequested(CheckRunRerequestedInput {
            head_sha: HEAD_SHA.to_string(),
            org: setup_org(ORG),
            pr_numbers: vec![PR_NUMBER],
            received_at: Instant::now(),
        })
        .await;
    assert_eq!(
        pr_rx.try_recv(),
        Ok(Job::Validate(ValidateInput {
            pr_head_owner: Some(FORK_OWNER.to_string()),
            pr_head_repo: Some(FORK_REPO.to_string()),
            ..setup_validate_input()
        }))
    );
}

#[tokio::test]
async fn test_handle_check_run_rerequested_retriggers_validation_for_fork_pr_via_commit_lookup() {
    let mut gh_fork = MockGH::new();
    gh_fork
        .expect_list_commit_prs()
        .withf(|ctx, sha| is_org_ctx(ctx) && sha == HEAD_SHA)
        .times(1)
        .returning(|_, _| {
            Box::pin(future::ready(Ok(vec![AssociatedPullRequest {
                base_ref: BASE_REF.to_string(),
                head_ref: HEAD_REF.to_string(),
                head_sha: HEAD_SHA.to_string(),
                is_open: true,
                number: PR_NUMBER,
                head_owner: Some(FORK_OWNER.to_string()),
                head_repo: Some(FORK_REPO.to_string()),
                updated_at: None,
            }])))
        });
    gh_fork.expect_list_pr_files().times(1).returning(|_, _| {
        Box::pin(future::ready(Ok(PrFiles {
            files: vec![CONFIG_PATH.to_string()],
            is_truncated: false,
        })))
    });
    gh_fork
        .expect_create_check_run()
        .withf(|ctx, body| is_org_ctx(ctx) && is_in_progress_check(body))
        .times(1)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    setup_triage_ctx(gh_fork, pr_tx)
        .handle_check_run_rerequested(CheckRunRerequestedInput {
            head_sha: HEAD_SHA.to_string(),
            org: setup_org(ORG),
            pr_numbers: vec![],
            received_at: Instant::now(),
        })
        .await;
    assert_eq!(
        pr_rx.try_recv(),
        Ok(Job::Validate(ValidateInput {
            pr_head_owner: Some(FORK_OWNER.to_string()),
            pr_head_repo: Some(FORK_REPO.to_string()),
            ..setup_validate_input()
        }))
    );
}

#[tokio::test(start_paused = true)]
async fn test_handle_validate_job_defers_check_publication_on_rate_limit_without_rerunning_validation() {
    // Validation succeeds and comment posts once; check publication hits a
    // rate limit on first attempt and succeeds on the deferred retry
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
    let mut svc = MockService::new();
    svc.expect_get_changes_summary()
        .times(1)
        .returning(|_, _, _| Ok(setup_summary(vec![], BaseRefConfigStatus::Valid)));

    let mut seq = Sequence::new();
    let mut gh = MockGH::new();
    gh.expect_post_comment()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _, _| Box::pin(future::ready(Ok(1))));
    gh.expect_create_check_run()
        .withf(|_, body| is_success_check(body))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| {
            Box::pin(future::ready(Err(Error::new(RateLimitError::primary(
                Some(Duration::from_secs(10)),
                "primary limit",
            )))))
        });
    gh.expect_create_check_run()
        .withf(|_, body| is_success_check(body))
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| Box::pin(future::ready(Ok(()))));

    let deferred_retries = DeferredRetries::new();
    let (pr_tx, _pr_rx) = mpsc::unbounded_channel();
    let worker = OrgWorker::new(JobsContext {
        cancel_jobs: CancellationToken::new(),
        clean_tracker: CleanReconcileTracker::new(),
        db: Arc::new(MockDB::new()),
        deferred_retries: deferred_retries.clone(),
        gh: Arc::new(gh),
        ghc,
        latest_pr_heads: LatestPrHeads::new(),
        pr_jobs_tx: pr_tx,
        reconcile_frequency: ONE_HOUR,
        services: HashMap::from([(GITHUB, Arc::new(svc) as DynServiceHandler)]),
        stop_intake: CancellationToken::new(),
        stop_workers: CancellationToken::new(),
        transport_cancel: CancellationToken::new(),
    });

    worker
        .handle_validate_job(setup_validate_input(), CancellationToken::new())
        .await
        .unwrap();
    deferred_retries.join_all().await;
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_retries_failed_org_before_remaining_sweep_slots_without_starving_peers() {
    // Setup 3 never-reconciled orgs with 6h frequency (2h slot spacing)
    let mut db = MockDB::new();
    db.expect_get_reconciliation_schedule_state()
        .returning(|| Box::pin(future::ready(Ok(HashMap::new()))));

    let clean_tracker = CleanReconcileTracker::new();
    let orgs = [setup_org("alpha"), setup_org("beta"), setup_org("gamma")];
    let (periodic_tx, mut periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    let start = Instant::now();
    let scheduler_done = scheduler(
        Arc::new(db),
        clean_tracker.clone(),
        periodic_tx,
        stop_intake.clone(),
        &orgs,
        Duration::from_hours(6),
    );

    // At t=0, "alpha" runs and fails
    let first = periodic_rx.recv().await.unwrap();
    assert_eq!(first.job.org_name(), "alpha");
    first.done.unwrap().send(PeriodicJobOutcome::Failed).unwrap();

    // "alpha" must retry after 15m backoff instead of waiting 4h for beta+gamma
    let second = timeout(Duration::from_mins(20), periodic_rx.recv())
        .await
        .expect("alpha should retry after 15m backoff")
        .unwrap();
    assert_eq!(second.job.org_name(), "alpha");
    assert!(start.elapsed() >= Duration::from_mins(15));
    assert!(start.elapsed() < Duration::from_mins(20));
    clean_tracker.record_outcome("alpha", OffsetDateTime::now_utc(), true);
    second.done.unwrap().send(PeriodicJobOutcome::Clean).unwrap();

    // "beta" still runs on its 2h periodic slot without starvation
    let third = timeout(Duration::from_hours(3), periodic_rx.recv())
        .await
        .expect("beta should run at its 2h slot")
        .unwrap();
    assert_eq!(third.job.org_name(), "beta");
    third.done.unwrap().send(PeriodicJobOutcome::Clean).unwrap();

    stop_intake.cancel();
    assert!(future::join_all(scheduler_done).await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test(start_paused = true)]
async fn test_scheduler_wakes_on_failed_pr_reconciliation_during_clean_sleep() {
    // Setup single org cleanly reconciled 2h ago (inside 6h frequency, so the
    // scheduler enters a ~4h sleep)
    let now = OffsetDateTime::now_utc();
    let clean_ts = now - ::time::Duration::hours(2);
    let mut db = MockDB::new();
    db.expect_get_reconciliation_schedule_state().returning(move || {
        let map = HashMap::from([(
            ORG.to_string(),
            OrgReconciliationStatus {
                last_attempt_at: Some(clean_ts),
                last_clean_at: Some(clean_ts),
            },
        )]);
        Box::pin(future::ready(Ok(map)))
    });

    let clean_tracker = CleanReconcileTracker::new();
    let (periodic_tx, mut periodic_rx) = mpsc::channel(1);
    let stop_intake = CancellationToken::new();
    let scheduler_done = scheduler(
        Arc::new(db),
        clean_tracker.clone(),
        periodic_tx,
        stop_intake.clone(),
        &[setup_org(ORG)],
        Duration::from_hours(6),
    );

    // Let scheduler enter its 4h clean-period sleep
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(periodic_rx.try_recv().is_err());

    // Record a newer PR reconciliation failure past the 15m backoff while the
    // scheduler is asleep; it must wake immediately instead of sleeping 4h
    clean_tracker.record_outcome(ORG, now - ::time::Duration::minutes(20), false);

    let job = timeout(Duration::from_secs(5), periodic_rx.recv())
        .await
        .expect("scheduler should wake immediately on failed PR reconciliation")
        .unwrap();
    assert_eq!(job.job.org_name(), ORG);
    job.done.unwrap().send(PeriodicJobOutcome::Clean).unwrap();

    stop_intake.cancel();
    assert!(future::join_all(scheduler_done).await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn test_triage_worker_isolates_rate_limited_installation_from_healthy_installation() {
    // Block installation 1's triage while letting installation 2 complete immediately
    let release_inst_1 = CancellationToken::new();
    let release_clone = release_inst_1.clone();
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().returning(move |ctx, _| {
        if ctx.inst_id == INSTALLATION_ID {
            let release = release_clone.clone();
            Box::pin(async move {
                release.cancelled().await;
                Ok(PrFiles {
                    files: vec![CONFIG_PATH.to_string()],
                    is_truncated: false,
                })
            })
        } else {
            Box::pin(future::ready(Ok(PrFiles {
                files: vec![CONFIG_PATH.to_string()],
                is_truncated: false,
            })))
        }
    });
    gh.expect_create_check_run().returning(|_, _| Box::pin(future::ready(Ok(()))));

    let (triage_tx, triage_rx) = mpsc::channel(16);
    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let stop_intake = CancellationToken::new();
    let mut triage_ctx = setup_triage_ctx(gh, pr_tx);
    triage_ctx.checks.stop_intake = stop_intake.clone();
    let worker = triage_worker(triage_ctx, triage_rx);

    // Send 4 deliveries for blocked installation 1 (equals TRIAGE_MAX_CONCURRENCY)
    for pr_num in 1..=4 {
        let mut input = setup_triage_input(PullRequestEventAction::Opened);
        input.pr_number = pr_num;
        triage_tx.send(TriageInput::PullRequest(input)).await.unwrap();
    }

    // Send a 5th delivery for healthy installation 2
    let mut other_org = setup_org(OTHER_ORG);
    other_org.installation_id = 2;
    let mut other_input = setup_triage_input(PullRequestEventAction::Opened);
    other_input.org = other_org;
    other_input.pr_number = 99;
    triage_tx.send(TriageInput::PullRequest(other_input)).await.unwrap();

    // Installation 2 must finish immediately without waiting on installation 1
    let job = timeout(Duration::from_secs(1), pr_rx.recv())
        .await
        .expect("healthy installation triage should not be blocked by installation 1")
        .unwrap();
    assert_eq!(job.org_name(), OTHER_ORG);

    // Unblock installation 1 and shut down cleanly
    release_inst_1.cancel();
    stop_intake.cancel();
    worker.await.unwrap();
}

#[test]
fn test_latest_pr_heads_handles_same_second_multihop_and_same_sha_redelivery() {
    let tracker = LatestPrHeads::new();
    let ts = OffsetDateTime::UNIX_EPOCH + ::time::Duration::seconds(1_700_000_000);

    // Same-second chain A -> B -> C; redelivering A afterward must be rejected
    let gen_a = tracker.record(ORG, PR_NUMBER, "sha-a".to_string(), Some(ts), None);
    let gen_b = tracker.record(
        ORG,
        PR_NUMBER,
        "sha-b".to_string(),
        Some(ts),
        Some("sha-a".to_string()),
    );
    let gen_c = tracker.record(
        ORG,
        PR_NUMBER,
        "sha-c".to_string(),
        Some(ts),
        Some("sha-b".to_string()),
    );
    assert!(gen_a.is_some() && gen_b.is_some() && gen_c.is_some());
    assert_eq!(
        tracker.record(ORG, PR_NUMBER, "sha-a".to_string(), Some(ts), None),
        None
    );
    assert!(!tracker.is_superseded(ORG, PR_NUMBER, "sha-c"));

    // Redelivering the current head C with an older timestamp (e.g. after a
    // re-request recorded a newer API updated_at) is accepted and bumps generation
    let rerun_ts = ts + ::time::Duration::seconds(30);
    let gen_rerun = tracker.record(ORG, PR_NUMBER, "sha-c".to_string(), Some(rerun_ts), None).unwrap();
    let gen_redeliver = tracker.record(ORG, PR_NUMBER, "sha-c".to_string(), Some(ts), None).unwrap();
    assert!(gen_redeliver > gen_rerun);
    assert!(tracker.is_superseded_attempt(ORG, PR_NUMBER, "sha-c", Some(gen_rerun)));
    assert!(!tracker.is_superseded_attempt(ORG, PR_NUMBER, "sha-c", Some(gen_redeliver)));

    // Missing timestamps still reject immediate predecessor redelivery
    let tracker_no_ts = LatestPrHeads::new();
    assert!(
        tracker_no_ts
            .record(
                ORG,
                PR_NUMBER,
                "sha-2".to_string(),
                None,
                Some("sha-1".to_string())
            )
            .is_some()
    );
    assert_eq!(
        tracker_no_ts.record(ORG, PR_NUMBER, "sha-1".to_string(), None, None),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn test_deferred_check_publication_is_skipped_after_same_sha_rerun() {
    // Record attempt 1 on HEAD_SHA
    let latest_pr_heads = LatestPrHeads::new();
    let gen1 = latest_pr_heads.record(ORG, PR_NUMBER, HEAD_SHA.to_string(), None, None);

    // First publication hits a rate limit; retry must never fire after rerun
    let mut gh = MockGH::new();
    gh.expect_create_check_run().times(1).returning(|_, _| {
        Box::pin(future::ready(Err(Error::new(RateLimitError::primary(
            Some(Duration::from_secs(10)),
            "primary limit",
        )))))
    });

    let deferred_retries = DeferredRetries::new();
    let stop_intake = CancellationToken::new();
    let transport_cancel = CancellationToken::new();
    let check_body = github::new_checks_create_request(
        HEAD_SHA.to_string(),
        Some(JobStatus::Completed),
        Some(ChecksCreateRequestConclusion::Success),
        CHECK_SUCCEEDED,
    );
    let publisher = CheckRunPublisher {
        deferred_retries: deferred_retries.clone(),
        gh: Arc::new(gh),
        latest_pr_heads: latest_pr_heads.clone(),
        stop_intake,
        transport_cancel,
    };
    let org = setup_org(ORG);
    let target = CheckRunTarget {
        head_sha: HEAD_SHA,
        org: &org,
        pr_number: PR_NUMBER,
        attempt_generation: gen1,
    };
    publisher.publish_with_retry(&target, check_body).await.unwrap();

    // Simulate a rerun on the same HEAD_SHA before attempt 1's delay elapses
    latest_pr_heads.record(ORG, PR_NUMBER, HEAD_SHA.to_string(), None, None);
    deferred_retries.join_all().await;
}

#[tokio::test(start_paused = true)]
async fn test_org_worker_defers_queued_periodic_job_immediately_when_pr_job_is_ready() {
    // Setup a gated service so the PR job stays blocked until released
    let (svc, mut started_rx) = GatedService::new([ORG]);
    let mut db = MockDB::new();
    db.expect_register_reconciliation()
        .times(1)
        .returning(|_, _, _| Box::pin(future::ready(Ok(()))));

    // Populate BOTH pr_jobs_rx and periodic_jobs_rx before starting the worker
    let (pr_tx, pr_rx) = mpsc::unbounded_channel();
    let (periodic_tx, periodic_rx) = mpsc::channel(1);
    let (done_tx, done_rx) = oneshot::channel();
    pr_tx.send(setup_periodic_job(setup_org(ORG))).unwrap();
    periodic_tx
        .send(QueuedJob {
            job: setup_periodic_job(setup_org(ORG)),
            done: Some(done_tx),
        })
        .await
        .unwrap();

    let stop_intake = CancellationToken::new();
    let stop_worker = CancellationToken::new();
    let worker = setup_worker_with_tokens(
        db,
        MockGH::new(),
        Arc::new(FakeCoreGH::default()),
        [(GITHUB, svc.clone())],
        stop_intake.clone(),
        CancellationToken::new(),
    );
    let worker_done = worker.run(pr_rx, periodic_rx, stop_worker.clone(), OrgWorkerActivity::new());

    // Periodic job must receive DeferredBusy immediately while PR job is blocked
    assert_eq!(next_started(&mut started_rx).await, ORG);
    let outcome = timeout(Duration::from_secs(1), done_rx)
        .await
        .expect("queued periodic job should be deferred immediately")
        .unwrap();
    assert_eq!(outcome, PeriodicJobOutcome::DeferredBusy);

    svc.release(ORG);
    stop_intake.cancel();
    stop_worker.cancel();
    assert!(worker_done.await.is_ok());
}

#[tokio::test]
async fn test_triage_worker_recovers_installation_slot_after_task_panic() {
    // First triage call panics; second delivery for the same installation succeeds
    let mut seq = Sequence::new();
    let mut gh = MockGH::new();
    gh.expect_list_pr_files()
        .times(1)
        .in_sequence(&mut seq)
        .returning(|_, _| panic!("simulated triage panic"));
    gh.expect_list_pr_files().times(1).in_sequence(&mut seq).returning(|_, _| {
        Box::pin(future::ready(Ok(PrFiles {
            files: vec![CONFIG_PATH.to_string()],
            is_truncated: false,
        })))
    });
    gh.expect_create_check_run().times(1).returning(|_, _| Box::pin(future::ready(Ok(()))));

    let (triage_tx, triage_rx) = mpsc::channel(16);
    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let stop_intake = CancellationToken::new();
    let mut triage_ctx = setup_triage_ctx(gh, pr_tx);
    triage_ctx.checks.stop_intake = stop_intake.clone();
    let worker = triage_worker(triage_ctx, triage_rx);

    triage_tx
        .send(TriageInput::PullRequest(setup_triage_input(
            PullRequestEventAction::Opened,
        )))
        .await
        .unwrap();
    triage_tx
        .send(TriageInput::PullRequest(setup_triage_input(
            PullRequestEventAction::Opened,
        )))
        .await
        .unwrap();

    // Second delivery must still be triaged after the first panicked
    let job = timeout(Duration::from_secs(1), pr_rx.recv())
        .await
        .expect("second delivery should not be wedged by first task panic")
        .unwrap();
    assert_eq!(job, Job::Validate(setup_validate_input()));

    stop_intake.cancel();
    worker.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn test_handle_check_run_rerequested_defers_rate_limited_lookup() {
    // First get_pr hits a rate limit; deferred retry succeeds and enqueues validate
    let mut seq = Sequence::new();
    let mut gh = MockGH::new();
    gh.expect_get_pr().times(1).in_sequence(&mut seq).returning(|_, _| {
        Box::pin(future::ready(Err(Error::new(RateLimitError::primary(
            Some(Duration::from_secs(10)),
            "primary limit",
        )))))
    });
    gh.expect_get_pr().times(1).in_sequence(&mut seq).returning(|_, _| {
        Box::pin(future::ready(Ok(AssociatedPullRequest {
            base_ref: BASE_REF.to_string(),
            head_ref: HEAD_REF.to_string(),
            head_sha: HEAD_SHA.to_string(),
            is_open: true,
            number: PR_NUMBER,
            head_owner: None,
            head_repo: None,
            updated_at: None,
        })))
    });
    gh.expect_list_pr_files().times(1).returning(|_, _| {
        Box::pin(future::ready(Ok(PrFiles {
            files: vec![CONFIG_PATH.to_string()],
            is_truncated: false,
        })))
    });
    gh.expect_create_check_run().times(1).returning(|_, _| Box::pin(future::ready(Ok(()))));

    let deferred_retries = DeferredRetries::new();
    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let mut triage_ctx = setup_triage_ctx(gh, pr_tx);
    triage_ctx.checks.deferred_retries = deferred_retries.clone();
    triage_ctx
        .handle_check_run_rerequested(CheckRunRerequestedInput {
            head_sha: HEAD_SHA.to_string(),
            org: setup_org(ORG),
            pr_numbers: vec![PR_NUMBER],
            received_at: Instant::now(),
        })
        .await;

    // Wait for deferred lookup retry to complete
    deferred_retries.join_all().await;
    assert_eq!(pr_rx.try_recv(), Ok(Job::Validate(setup_validate_input())));
}

#[tokio::test]
async fn test_triage_worker_aborts_in_flight_tasks_on_transport_cancel() {
    // Setup a triage task that blocks indefinitely once started
    let started = Arc::new(tokio::sync::Notify::new());
    let started_clone = started.clone();
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().times(1).returning(move |_, _| {
        let notify = started_clone.clone();
        Box::pin(async move {
            notify.notify_one();
            future::pending().await
        })
    });
    gh.expect_create_check_run().never();

    // Launch triage worker and send one delivery
    let (triage_tx, triage_rx) = mpsc::channel(4);
    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let transport_cancel = CancellationToken::new();
    let mut triage_ctx = setup_triage_ctx(gh, pr_tx);
    triage_ctx.checks.transport_cancel = transport_cancel.clone();
    let worker = triage_worker(triage_ctx, triage_rx);

    triage_tx
        .send(TriageInput::PullRequest(setup_triage_input(
            PullRequestEventAction::Opened,
        )))
        .await
        .unwrap();
    started.notified().await;

    // Cancelling transport_cancel must abort the blocked task and stop the worker
    transport_cancel.cancel();
    timeout(Duration::from_secs(1), worker)
        .await
        .expect("triage worker should abort in-flight tasks promptly on transport cancel")
        .unwrap();
    assert_eq!(pr_rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[tokio::test]
async fn test_triage_worker_applies_backpressure_at_queue_capacity() {
    // Block the first delivery so queued items stay buffered inside the worker
    let started = Arc::new(tokio::sync::Notify::new());
    let started_clone = started.clone();
    let release = CancellationToken::new();
    let release_clone = release.clone();
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().returning(move |_, pr_number| {
        if pr_number == 1 {
            let notify = started_clone.clone();
            let release = release_clone.clone();
            Box::pin(async move {
                notify.notify_one();
                release.cancelled().await;
                Ok(PrFiles {
                    files: vec![CONFIG_PATH.to_string()],
                    is_truncated: false,
                })
            })
        } else {
            Box::pin(future::ready(Ok(PrFiles {
                files: vec![CONFIG_PATH.to_string()],
                is_truncated: false,
            })))
        }
    });
    gh.expect_create_check_run().returning(|_, _| Box::pin(future::ready(Ok(()))));

    // Fill the worker's internal buffer plus active slot to WEBHOOK_TRIAGE_QUEUE_CAPACITY
    let (triage_tx, triage_rx) = mpsc::channel(2);
    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let stop_intake = CancellationToken::new();
    let mut triage_ctx = setup_triage_ctx(gh, pr_tx);
    triage_ctx.checks.stop_intake = stop_intake.clone();
    let worker = triage_worker(triage_ctx, triage_rx);

    let total_items = WEBHOOK_TRIAGE_QUEUE_CAPACITY + 2;
    for pr_number in 1..=i64::try_from(WEBHOOK_TRIAGE_QUEUE_CAPACITY).unwrap() {
        let mut input = setup_triage_input(PullRequestEventAction::Opened);
        input.pr_number = pr_number;
        triage_tx.send(TriageInput::PullRequest(input)).await.unwrap();
    }
    started.notified().await;

    // Wait until the worker has pulled all capacity items into its internal queue
    timeout(Duration::from_secs(1), async {
        while triage_tx.capacity() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker should pull up to capacity items");

    // Further deliveries must now fill triage_rx without being pulled until capacity frees
    for offset in 1..=2i64 {
        let mut input = setup_triage_input(PullRequestEventAction::Opened);
        input.pr_number = i64::try_from(WEBHOOK_TRIAGE_QUEUE_CAPACITY).unwrap() + offset;
        triage_tx.try_send(TriageInput::PullRequest(input)).unwrap();
    }
    tokio::task::yield_now().await;
    assert!(
        triage_tx
            .try_send(TriageInput::PullRequest(setup_triage_input(
                PullRequestEventAction::Opened,
            )))
            .is_err(),
        "triage_rx should stay full once internal buffer reaches WEBHOOK_TRIAGE_QUEUE_CAPACITY"
    );

    // Release the blocked delivery and verify every buffered item drains cleanly
    release.cancel();
    stop_intake.cancel();
    timeout(Duration::from_secs(5), worker)
        .await
        .expect("triage worker should drain all buffered items")
        .unwrap();

    let mut received = 0;
    while pr_rx.try_recv().is_ok() {
        received += 1;
    }
    assert_eq!(received, total_items);
}

#[tokio::test]
async fn test_triage_worker_drains_buffered_deliveries_on_stop_intake() {
    // Buffer multiple deliveries before signalling stop_intake
    let mut gh = MockGH::new();
    gh.expect_list_pr_files().times(3).returning(|_, _| {
        Box::pin(future::ready(Ok(PrFiles {
            files: vec![CONFIG_PATH.to_string()],
            is_truncated: false,
        })))
    });
    gh.expect_create_check_run().times(3).returning(|_, _| Box::pin(future::ready(Ok(()))));

    let (triage_tx, triage_rx) = mpsc::channel(8);
    for pr_number in 1..=3 {
        let mut input = setup_triage_input(PullRequestEventAction::Opened);
        input.pr_number = pr_number;
        triage_tx.send(TriageInput::PullRequest(input)).await.unwrap();
    }

    // Cancel stop_intake before starting the worker so all 3 items are drained via try_recv
    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let stop_intake = CancellationToken::new();
    stop_intake.cancel();
    let mut triage_ctx = setup_triage_ctx(gh, pr_tx);
    triage_ctx.checks.stop_intake = stop_intake;
    let worker = triage_worker(triage_ctx, triage_rx);

    timeout(Duration::from_secs(1), worker)
        .await
        .expect("triage worker should exit after draining buffered deliveries")
        .unwrap();

    let mut pr_numbers = Vec::new();
    while let Ok(Job::Validate(input)) = pr_rx.try_recv() {
        pr_numbers.push(input.pr_number);
    }
    assert_eq!(pr_numbers, vec![1, 2, 3]);
}

#[tokio::test]
async fn test_triage_worker_fails_closed_in_flight_check_when_cancel_jobs_fires() {
    // Setup a real FakeGitHub + ClientPool where the in-progress check POST is
    // already on the wire (delayed) when cancel_jobs fires while transport_cancel
    // stays open for the cleanup grace window
    let files_path = format!("/repos/{ORG}/{REPO}/pulls/{PR_NUMBER}/files");
    let check_runs_path = format!("/repos/{ORG}/{REPO}/check-runs");
    let fake = clowarden_core::github::http_fake::FakeGitHub::start(vec![
        clowarden_core::github::http_fake::Route::new(
            "GET",
            &files_path,
            200,
            serde_json::json!([{
                "filename": CONFIG_PATH,
                "status": "modified",
                "sha": "abc",
                "blob_url": "",
                "raw_url": "",
                "contents_url": "",
                "additions": 1,
                "deletions": 0,
                "changes": 1
            }])
            .to_string(),
        ),
        clowarden_core::github::http_fake::Route::once("POST", &check_runs_path, 201, r#"{"output": {}}"#)
            .with_delay(Duration::from_millis(80)),
        clowarden_core::github::http_fake::Route::new("POST", &check_runs_path, 201, r#"{"output": {}}"#),
    ]);
    let policy = clowarden_core::github::RateLimitPolicy {
        jitter_max: Duration::ZERO,
        mutation_interval: Duration::ZERO,
        ..clowarden_core::github::RateLimitPolicy::default()
    };
    let cancel_jobs = CancellationToken::new();
    let transport_cancel = CancellationToken::new();
    let pool = fake.client_pool_with_shutdown(policy, transport_cancel.clone());
    let gh: DynGH = Arc::new(github::GHApi::new(pool));

    // Launch triage worker and deliver an opened PR event
    let (triage_tx, triage_rx) = mpsc::channel(4);
    let (pr_tx, mut pr_rx) = mpsc::unbounded_channel();
    let triage_ctx = TriageContext {
        cancel_jobs: cancel_jobs.clone(),
        checks: CheckRunPublisher {
            deferred_retries: DeferredRetries::new(),
            gh,
            latest_pr_heads: LatestPrHeads::new(),
            stop_intake: CancellationToken::new(),
            transport_cancel,
        },
        pr_jobs_tx: pr_tx,
    };
    let worker = triage_worker(triage_ctx, triage_rx);

    triage_tx
        .send(TriageInput::PullRequest(setup_triage_input(
            PullRequestEventAction::Opened,
        )))
        .await
        .unwrap();

    // Wait until FakeGitHub receives the in-progress POST, then fire cancel_jobs
    // while the response is still delayed
    timeout(Duration::from_secs(2), async {
        loop {
            if fake.requests().iter().any(|r| r.method == "POST" && r.path == check_runs_path) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("in-progress check POST should reach FakeGitHub");
    cancel_jobs.cancel();

    timeout(Duration::from_secs(2), worker)
        .await
        .expect("triage worker should finish unwinding within grace window")
        .unwrap();

    // Check triage posted the fail-closed check instead of leaving in_progress stuck
    assert_eq!(pr_rx.try_recv(), Err(TryRecvError::Disconnected));
    let posts: Vec<_> = fake
        .requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path == check_runs_path)
        .collect();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0].body.as_ref().unwrap()["status"], "in_progress");
    assert_eq!(posts[1].body.as_ref().unwrap()["status"], "completed");
    assert_eq!(posts[1].body.as_ref().unwrap()["conclusion"], "failure");
    assert_eq!(
        posts[1].body.as_ref().unwrap()["output"]["summary"],
        INTERRUPTED_VALIDATION_MSG
    );
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
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, head)]);
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
    let result = worker.handle_validate_job(setup_validate_input(), CancellationToken::new()).await;

    // Check the job fails with the directory error
    let err = result.unwrap_err().to_string();
    assert!(err.contains(expected_err), "unexpected error: {err}");
}

/// Check that a job hanging in its service is timed out, so that the worker can
/// process the next one.
async fn check_worker_moves_on_after_timeout(hanging_job: Job) {
    // Setup a service that hangs until released (ignoring cooperative cancel)
    let (svc, mut started_rx) = GatedService::new([ORG]);

    // Setup configuration, audit and GitHub expectations
    let ghc = FakeCoreGH::new([(BASE_REF, Ok(BASE_CONFIG)), (HEAD_SHA, Ok(HEAD_CONFIG))]);
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
    let (pr_tx, pr_rx) = mpsc::unbounded_channel();
    let (_periodic_tx, periodic_rx) = mpsc::channel(1);
    pr_tx.send(hanging_job).unwrap();
    pr_tx.send(setup_periodic_job(setup_org(ORG))).unwrap();
    let stop_intake = CancellationToken::new();
    let stop_worker = CancellationToken::new();
    let worker = setup_worker_with_tokens(
        db,
        gh,
        ghc,
        [(GITHUB, svc.clone())],
        stop_intake.clone(),
        CancellationToken::new(),
    );
    let worker_done = worker.run(pr_rx, periodic_rx, stop_worker.clone(), OrgWorkerActivity::new());

    // Check the second job starts once the first one hits the hard safety timeout
    assert_eq!(next_started(&mut started_rx).await, ORG);
    let first_job_started_at = Instant::now();
    assert_eq!(next_started(&mut started_rx).await, ORG);
    assert_eq!(
        first_job_started_at.elapsed(),
        Duration::from_secs(JOB_TIMEOUT + JOB_HARD_TIMEOUT_GRACE)
    );

    // Finish the second job and stop the worker
    svc.release(ORG);
    stop_intake.cancel();
    stop_worker.cancel();
    assert!(worker_done.await.is_ok());
}

/// Check if the check run provided fails the PR head.
fn is_failure_check(body: &ChecksCreateRequest) -> bool {
    is_pr_head_check(body, &ChecksCreateRequestConclusion::Failure, CHECK_FAILED)
}

/// Check if the check run provided marks the PR head as in progress.
fn is_in_progress_check(body: &ChecksCreateRequest) -> bool {
    body.conclusion.is_none()
        && body.head_sha == HEAD_SHA
        && body.name == "CLOWarden"
        && body.status == Some(JobStatus::InProgress)
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

/// Create a triage context wired to the mock GitHub client and PR jobs sender.
fn setup_triage_ctx(gh: MockGH, pr_jobs_tx: mpsc::UnboundedSender<Job>) -> TriageContext {
    TriageContext {
        cancel_jobs: CancellationToken::new(),
        checks: CheckRunPublisher {
            deferred_retries: DeferredRetries::new(),
            gh: Arc::new(gh),
            latest_pr_heads: LatestPrHeads::new(),
            stop_intake: CancellationToken::new(),
            transport_cancel: CancellationToken::new(),
        },
        pr_jobs_tx,
    }
}

/// Create a compact triage input for the test PR and action provided.
fn setup_triage_input(action: PullRequestEventAction) -> PullRequestTriageInput {
    PullRequestTriageInput {
        action,
        org: setup_org(ORG),
        pr_head_ref: HEAD_REF.to_string(),
        pr_head_sha: HEAD_SHA.to_string(),
        pr_number: PR_NUMBER,
        received_at: Instant::now(),
        pr_before_sha: None,
        pr_created_by: None,
        pr_head_owner: None,
        pr_head_repo: None,
        pr_merged_at: None,
        pr_merged_by: None,
        pr_updated_at: None,
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
        retry_attempt: 0,
    }
}

/// Create an organization worker using the dependencies provided.
fn setup_worker<const N: usize>(
    db: MockDB,
    gh: MockGH,
    ghc: Arc<FakeCoreGH>,
    services: [(ServiceName, DynServiceHandler); N],
) -> OrgWorker {
    let (pr_jobs_tx, _pr_jobs_rx) = mpsc::unbounded_channel();
    setup_worker_with_pr_tx(db, gh, ghc, services, pr_jobs_tx)
}

/// Create an organization worker wired to the PR jobs sender provided.
fn setup_worker_with_pr_tx<const N: usize>(
    db: MockDB,
    gh: MockGH,
    ghc: Arc<FakeCoreGH>,
    services: [(ServiceName, DynServiceHandler); N],
    pr_jobs_tx: mpsc::UnboundedSender<Job>,
) -> OrgWorker {
    OrgWorker::new(JobsContext {
        cancel_jobs: CancellationToken::new(),
        clean_tracker: CleanReconcileTracker::new(),
        db: Arc::new(db),
        deferred_retries: DeferredRetries::new(),
        gh: Arc::new(gh),
        ghc,
        latest_pr_heads: LatestPrHeads::new(),
        pr_jobs_tx,
        reconcile_frequency: ONE_HOUR,
        services: HashMap::from(services),
        stop_intake: CancellationToken::new(),
        stop_workers: CancellationToken::new(),
        transport_cancel: CancellationToken::new(),
    })
}

/// Create an organization worker using explicit shutdown tokens.
fn setup_worker_with_tokens<const N: usize>(
    db: MockDB,
    gh: MockGH,
    ghc: Arc<FakeCoreGH>,
    services: [(ServiceName, DynServiceHandler); N],
    stop_intake: CancellationToken,
    transport_cancel: CancellationToken,
) -> OrgWorker {
    let (pr_jobs_tx, _pr_jobs_rx) = mpsc::unbounded_channel();
    OrgWorker::new(JobsContext {
        cancel_jobs: CancellationToken::new(),
        clean_tracker: CleanReconcileTracker::new(),
        db: Arc::new(db),
        deferred_retries: DeferredRetries::new(),
        gh: Arc::new(gh),
        ghc,
        latest_pr_heads: LatestPrHeads::new(),
        pr_jobs_tx,
        reconcile_frequency: ONE_HOUR,
        services: HashMap::from(services),
        stop_intake,
        stop_workers: CancellationToken::new(),
        transport_cancel,
    })
}

/// Service handler that waits for `job_ctx.cancel_token` and then returns
/// cancellation / partial outcome, allowing end-to-end shutdown cleanup tests.
struct CancelAwareService {
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ServiceHandler for CancelAwareService {
    /// [`ServiceHandler::get_changes_summary`].
    async fn get_changes_summary(
        &self,
        _org: &Organization,
        _head_src: &Source,
        job_ctx: &JobContext,
    ) -> Result<ChangesSummary> {
        self.started.notify_one();
        job_ctx.cancel_token.cancelled().await;
        Err(Error::new(clowarden_core::github::CancelledError))
    }

    /// [`ServiceHandler::reconcile`].
    async fn reconcile(&self, _org: &Organization, job_ctx: &JobContext) -> ReconcileOutcome {
        self.started.notify_one();
        job_ctx.cancel_token.cancelled().await;
        ReconcileOutcome {
            changes_applied: vec![setup_change_applied()],
            error: Some(Error::new(clowarden_core::github::CancelledError)),
        }
    }
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
    /// [`ServiceHandler::get_changes_summary`].
    async fn get_changes_summary(
        &self,
        org: &Organization,
        _head_src: &Source,
        _job_ctx: &JobContext,
    ) -> Result<ChangesSummary> {
        self.enter(org).await;
        Ok(setup_summary(vec![], BaseRefConfigStatus::Valid))
    }

    /// [`ServiceHandler::reconcile`].
    async fn reconcile(&self, org: &Organization, _job_ctx: &JobContext) -> ReconcileOutcome {
        self.enter(org).await;
        ReconcileOutcome::ok(vec![])
    }
}

mock! {
    Service {}

    #[async_trait]
    impl ServiceHandler for Service {
        async fn get_changes_summary(
            &self,
            org: &Organization,
            head_src: &Source,
            job_ctx: &JobContext,
        ) -> Result<ChangesSummary>;
        async fn reconcile(&self, org: &Organization, job_ctx: &JobContext) -> ReconcileOutcome;
    }
}
