use crate::{
    db::DynDB,
    directory::Directory,
    github::{self, DynGH},
    multierror::MultiError,
    services::{BaseRefConfigStatus, ChangesApplied, ChangesSummary, DynServiceHandler, ServiceName},
    tmpl,
};
use ::time::OffsetDateTime;
use anyhow::{Error, Result};
use askama::Template;
use config::Config;
use octorust::types::{ChecksCreateRequestConclusion, JobStatus, PullRequestData};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};
use tracing::{debug, error, instrument};

/// Represents a job to be executed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Job {
    /// A reconcile job verifies if the desired state as described in the
    /// configuration files matches the actual state in the external services,
    /// applying the necessary changes. This work is delegated on services
    /// handlers, one for each of the external services. It can be triggered
    /// periodically or manually from a pull request. When it's triggered from
    /// a pull request, any feedback will be published to it in the form of
    /// comments.
    Reconcile(ReconcileInput),

    /// A validate job verifies that the proposed changes to the configuration
    /// files in a pull request are valid, providing feedback to address issues
    /// whenever possible, as well as a summary of changes to facilitate
    /// reviews.
    Validate(ValidateInput),
}

/// Information required to process a reconcile job.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReconcileInput {
    pub pr_number: Option<i64>,
    pub pr_created_by: Option<String>,
    pub pr_merged_by: Option<String>,
    pub pr_merged_at: Option<OffsetDateTime>,
}

impl From<PullRequestData> for ReconcileInput {
    fn from(pr: PullRequestData) -> Self {
        let mut input = ReconcileInput {
            pr_number: Some(pr.number),
            pr_created_by: pr.user.map(|u| u.login),
            pr_merged_by: pr.merged_by.map(|u| u.login),
            ..Default::default()
        };
        if let Some(pr_merged_at) = pr.merged_at {
            match OffsetDateTime::from_unix_timestamp(pr_merged_at.timestamp()) {
                Ok(pr_merged_at) => input.pr_merged_at = Some(pr_merged_at),
                Err(_) => error!(pr.number, ?pr_merged_at, "invalid merged_at value"),
            }
        }
        input
    }
}

/// Information required to process a validate job.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ValidateInput {
    pub pr_number: i64,
    pub pr_head_ref: String,
    pub pr_head_sha: String,
}

impl From<PullRequestData> for ValidateInput {
    fn from(pr: PullRequestData) -> Self {
        ValidateInput {
            pr_number: pr.number,
            pr_head_ref: pr.head.ref_,
            pr_head_sha: pr.head.sha,
        }
    }
}

/// A jobs handler is in charge of executing the received jobs.
pub(crate) struct Handler {
    cfg: Arc<Config>,
    db: DynDB,
    gh: DynGH,
    services: HashMap<ServiceName, DynServiceHandler>,
}

impl Handler {
    /// Create a new handler instance.
    pub(crate) fn new(
        cfg: Arc<Config>,
        db: DynDB,
        gh: DynGH,
        services: HashMap<ServiceName, DynServiceHandler>,
    ) -> Self {
        Self {
            cfg,
            db,
            gh,
            services,
        }
    }

    /// Spawn a new task to process jobs received on the jobs channel. The task
    /// will stop when notified on the stop channel provided.
    pub(crate) fn start(
        self,
        mut jobs_rx: mpsc::UnboundedReceiver<Job>,
        mut stop_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;

                    // Pick next job from the queue and process it
                    Some(job) = jobs_rx.recv() => {
                        match job {
                            Job::Reconcile(input) => _ = self.handle_reconcile_job(input).await,
                            Job::Validate(input) => _ = self.handle_validate_job(input).await,
                        }
                    }

                    // Exit if the handler has been asked to stop
                    _ = stop_rx.recv() => {
                        break
                    }
                }
            }
        })
    }

    /// Reconcile job handler.
    #[instrument(skip_all, err(Debug))]
    async fn handle_reconcile_job(&self, input: ReconcileInput) -> Result<()> {
        let mut changes_applied: HashMap<ServiceName, ChangesApplied> = HashMap::new();
        let mut errors: HashMap<ServiceName, Error> = HashMap::new();

        // Reconcile services state
        for (service_name, service_handler) in &self.services {
            debug!(service_name, "reconciling state");
            match service_handler.reconcile().await {
                Ok(service_changes_applied) => {
                    changes_applied.insert(service_name, service_changes_applied);
                }
                Err(err) => {
                    errors.insert(service_name, err);
                }
            }
        }

        // Register changes applied during reconciliation in database
        if let Err(err) = self.db.register_reconciliation(&input, &changes_applied, &errors).await {
            error!(?err, "error registering reconciliation in database");
        }

        // Post reconciliation completed comment if the job was created from a PR
        if let Some(pr_number) = input.pr_number {
            let comment_body = tmpl::ReconciliationCompleted::new(&changes_applied, &errors).render()?;
            if let Err(err) = self.gh.post_comment(pr_number, &comment_body).await {
                error!(?err, "error posting reconciliation comment");
            }
        }

        // Log changes applied and errors
        for (service_name, error) in &errors {
            debug!(?error, service = service_name, "reconciliation failed");
        }
        for (service_name, changes_applied) in &changes_applied {
            for entry in changes_applied {
                let msg = if entry.error.is_none() {
                    "change applied"
                } else {
                    "something went wrong applying change"
                };
                let details = entry.change.details();
                debug!(
                    service = service_name,
                    kind = details.kind,
                    extra = serde_json::to_string(&details.extra)?,
                    error = entry.error,
                    "{msg}"
                );
            }
        }

        Ok(())
    }

    /// Validate job handler.
    #[instrument(fields(pr_number = input.pr_number), skip_all, err(Debug))]
    async fn handle_validate_job(&self, input: ValidateInput) -> Result<()> {
        let mut merr = MultiError::new(None);

        // Directory configuration validation
        let directory_changes =
            match Directory::get_changes_summary(self.cfg.clone(), self.gh.clone(), &input.pr_head_ref).await
            {
                Ok(changes) => changes,
                Err(err) => {
                    merr.push(err);
                    ChangesSummary {
                        changes: vec![],
                        base_ref_config_status: BaseRefConfigStatus::Unknown,
                    }
                }
            };

        // Services configuration validation
        let mut services_changes: HashMap<ServiceName, ChangesSummary> = HashMap::new();
        if !merr.contains_errors() {
            for (service_name, service_handler) in &self.services {
                match service_handler.get_changes_summary(&input.pr_head_ref).await {
                    Ok(changes) => {
                        services_changes.insert(service_name, changes);
                    }
                    Err(err) => merr.push(err),
                }
            }
        }

        // Post validation completed comment and create check run
        let errors_found = merr.contains_errors();
        let err = Error::from(merr);
        let (comment_body, check_body) = match errors_found {
            true => {
                let comment_body = tmpl::ValidationFailed::new(&err).render()?;
                let check_body = github::new_checks_create_request(
                    input.pr_head_sha,
                    Some(JobStatus::Completed),
                    Some(ChecksCreateRequestConclusion::Failure),
                    "The configuration changes proposed are not valid",
                );
                (comment_body, check_body)
            }
            false => {
                let comment_body =
                    tmpl::ValidationSucceeded::new(&directory_changes, &services_changes).render()?;
                let check_body = github::new_checks_create_request(
                    input.pr_head_sha,
                    Some(JobStatus::Completed),
                    Some(ChecksCreateRequestConclusion::Success),
                    "The configuration changes proposed are valid",
                );
                (comment_body, check_body)
            }
        };
        self.gh.post_comment(input.pr_number, &comment_body).await?;
        self.gh.create_check_run(&check_body).await?;

        if errors_found {
            return Err(err);
        }
        Ok(())
    }
}

/// How often periodic reconcile jobs should be scheduled (in seconds).
const RECONCILE_FREQUENCY: u64 = 60 * 60;

/// A jobs scheduler is in charge of scheduling the execution of some jobs
/// periodically.
pub(crate) struct Scheduler;

impl Scheduler {
    /// Create a new scheduler instance.
    pub(crate) fn new() -> Self {
        Self {}
    }

    /// Spawn a new task to schedule jobs periodically.
    pub(crate) fn start(
        &self,
        jobs_tx: mpsc::UnboundedSender<Job>,
        mut stop_rx: broadcast::Receiver<()>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let reconcile_frequency = time::Duration::from_secs(RECONCILE_FREQUENCY);
            let mut reconcile = time::interval(reconcile_frequency);
            reconcile.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    biased;

                    // Exit if the scheduler has been asked to stop
                    _ = stop_rx.recv() => {
                        break
                    }

                    // Schedule reconcile job
                    _ = reconcile.tick() => {
                        _ = jobs_tx.send(Job::Reconcile(ReconcileInput::default()));
                    },
                }
            }
        })
    }
}
