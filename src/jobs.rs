use crate::{
    directory::Directory,
    github::{self, DynGH},
    tmpl,
};
use anyhow::Result;
use askama::Template;
use config::Config;
use octorust::types::{ChecksCreateRequestConclusion, JobStatus, PullRequestData};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};
use tracing::{debug, instrument};

/// How often periodic reconcile jobs should be scheduled (in seconds).
const RECONCILE_FREQUENCY: u64 = 60 * 60;

/// Represents a job to be executed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Job {
    /// A reconcile job verifies if the desired state as described in the
    /// configuration files matches the current state in the external services,
    /// applying the necessary changes. This work is delegated on plugins, one
    /// for each of the external services. It can be triggered periodically or
    /// manually from a pull request. When it's triggered from a pull request,
    /// any feedback will be published to it in the form of comments.
    Reconcile(Option<PullRequestData>),

    /// A validate job verifies that the proposed changes to the configuration
    /// files in a pull request are valid, providing feedback to address issues
    /// whenever possible, as well as a summary of changes to facilitate
    /// reviews.
    Validate(PullRequestData),
}

/// A jobs handler is in charge of executing the received jobs.
pub(crate) struct Handler {
    cfg: Arc<Config>,
    gh: DynGH,
}

impl Handler {
    /// Create a new handler instance.
    pub(crate) fn new(cfg: Arc<Config>, gh: DynGH) -> Self {
        Self { cfg, gh }
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
                            Job::Reconcile(pr) => _ = self.handle_reconcile_job(pr).await,
                            Job::Validate(pr) => _ = self.handle_validation_job(pr).await,
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
    async fn handle_reconcile_job(&self, _pr: Option<PullRequestData>) -> Result<()> {
        debug!("handling reconcile job (unimplemented)");
        Ok(())
    }

    /// Validation job handler.
    #[instrument(fields(pr_number = pr.number), skip_all, err(Debug))]
    async fn handle_validation_job(&self, pr: PullRequestData) -> Result<()> {
        // Validate configuration changes
        let directory_head =
            match Directory::new(self.cfg.clone(), self.gh.clone(), Some(&pr.head.ref_)).await {
                Ok(directory) => directory,
                Err(err) => {
                    // Validation failed, post results
                    let comment_body = tmpl::ValidationFailed::new(&err).render()?;
                    self.gh.post_comment(pr.number, &comment_body).await?;
                    let check_body = github::new_checks_create_request(
                        pr.head.sha,
                        Some(JobStatus::Completed),
                        Some(ChecksCreateRequestConclusion::Failure),
                        "The configuration changes proposed are not valid",
                    );
                    self.gh.create_check_run(&check_body).await?;
                    return Err(err);
                }
            };

        // Configuration changes are valid: calculate changes between the head
        // and base refs and post the results
        let changes = match Directory::new(self.cfg.clone(), self.gh.clone(), None).await {
            Ok(directory_base) => directory_base.changes(&directory_head),
            Err(_) => vec![],
        };
        let comment_body = tmpl::ValidationSucceeded::new(&changes).render()?;
        self.gh.post_comment(pr.number, &comment_body).await?;
        let check_body = github::new_checks_create_request(
            pr.head.sha,
            Some(JobStatus::Completed),
            Some(ChecksCreateRequestConclusion::Success),
            "The configuration changes proposed are valid",
        );
        self.gh.create_check_run(&check_body).await?;

        Ok(())
    }
}

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
                        _ = jobs_tx.send(Job::Reconcile(None));
                    },
                }
            }
        })
    }
}
