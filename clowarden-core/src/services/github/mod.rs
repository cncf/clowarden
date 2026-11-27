//! This module contains the implementation of the GitHub service handler.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Error, Result, format_err};
use as_any::Downcast;
use async_trait::async_trait;
use octorust::{ClientError, types::RepositoryInvitation};
use reqwest::StatusCode;
use tracing::{debug, warn};

use crate::{
    cfg::Organization,
    directory::{DirectoryChange, TeamName, UserName},
    github::{CancelledError, Source, is_cancelled_error, is_rate_limit_error},
    multierror::MultiError,
    services::{ChangeApplied, JobContext, ReconcileOutcome},
};

use super::{BaseRefConfigStatus, ChangesSummary, DynChange, ServiceHandler};

use self::{
    service::{Ctx, DynSvc},
    state::{Changes, RepositoryChange, RepositoryInvitationId, RepositoryName, Role, ServiceMetadata},
};

mod legacy;
pub mod service;
pub mod state;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;
pub use state::State;

/// GitHub's service name.
pub const SERVICE_NAME: &str = "github";

/// GitHub's service handler.
pub struct Handler {
    svc: DynSvc,
}

impl Handler {
    /// Create a new Handler instance.
    #[must_use]
    pub fn new(svc: DynSvc) -> Self {
        Self { svc }
    }

    /// Add a repository collaborator, deleting any expired invitation for the
    /// user first so GitHub creates a fresh invitation instead of accepting
    /// the request as a no-op. Returns the operation result and whether any
    /// expired invitation was deleted on GitHub before returning.
    async fn add_collaborator_or_reinvite(
        &self,
        ctx: &Ctx,
        invitations: &HashMap<RepositoryName, Vec<RepositoryInvitation>>,
        org_members: &[UserName],
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> (Result<()>, bool) {
        // Reject case-only username mismatches against expired invitations
        // before mutating so the next sweep cannot revoke and re-invite in a loop
        if let Some(canonical_login) =
            find_case_mismatched_expired_invitation(invitations, repo_name, user_name)
        {
            return (
                Err(format_err!(
                    "collaborator {user_name} in {repo_name} has invalid username casing, should be {canonical_login}"
                )),
                false,
            );
        }

        // Delete any expired invitations for this user before re-inviting so
        // GitHub does not keep the stale invitation in place
        let expired_invitation_ids: Vec<RepositoryInvitationId> =
            expired_repository_invitations(invitations, repo_name, user_name).collect();
        let mut mutated = false;
        for invitation_id in &expired_invitation_ids {
            match self.svc.remove_repository_invitation(ctx, repo_name, *invitation_id).await {
                Ok(()) => mutated = true,
                Err(err) if is_not_found_error(&err) => {}
                Err(err) => return (Err(err), mutated),
            }
        }

        // Send the collaborator request and verify GitHub did not return a
        // stale invitation or a 204 no-op for an outside-collaborator re-invite
        let invitation = match self.svc.add_repository_collaborator(ctx, repo_name, user_name, role).await {
            Ok(invitation) => invitation,
            Err(err) => return (Err(err), mutated),
        };
        let valid_response = match invitation {
            Some(invitation) => !invitation.expired && !expired_invitation_ids.contains(&invitation.id),
            None => expired_invitation_ids.is_empty() || org_members.contains(user_name),
        };
        if !valid_response {
            return (
                Err(format_err!(
                    "github did not create a fresh invitation for collaborator {user_name} in {repo_name}"
                )),
                mutated,
            );
        }

        (Ok(()), mutated)
    }

    /// Apply directory and repository changes sequentially, stopping early on
    /// cancellation or exhausted rate limits while preserving applied work.
    async fn apply_changes(
        &self,
        ctx: &Ctx,
        changes: Changes,
        invitations: &HashMap<RepositoryName, Vec<RepositoryInvitation>>,
        org_members: &[UserName],
        changes_applied: &mut Vec<ChangeApplied>,
    ) -> Result<()> {
        self.apply_directory_changes(ctx, changes.directory, changes_applied).await?;
        self.apply_repository_changes(
            ctx,
            changes.repositories,
            invitations,
            org_members,
            changes_applied,
        )
        .await
    }

    /// Apply a single directory change to GitHub, returning `None` when the
    /// change kind is ignored by the GitHub handler.
    async fn apply_directory_change(&self, ctx: &Ctx, change: &DirectoryChange) -> Option<Result<()>> {
        let res = match change {
            DirectoryChange::TeamAdded(team) => self.svc.add_team(ctx, team).await,
            DirectoryChange::TeamMaintainerAdded(team_name, user_name) => {
                self.svc.add_team_maintainer(ctx, team_name, user_name).await
            }
            DirectoryChange::TeamMaintainerRemoved(team_name, user_name) => {
                self.svc.remove_team_maintainer(ctx, team_name, user_name).await
            }
            DirectoryChange::TeamMemberAdded(team_name, user_name) => {
                self.svc.add_team_member(ctx, team_name, user_name).await
            }
            DirectoryChange::TeamMemberRemoved(team_name, user_name) => {
                self.svc.remove_team_member(ctx, team_name, user_name).await
            }
            DirectoryChange::TeamRemoved(team_name) => self.svc.remove_team(ctx, team_name).await,
            DirectoryChange::UserAdded(_)
            | DirectoryChange::UserRemoved(_)
            | DirectoryChange::UserUpdated(_) => return None,
        };
        Some(res)
    }

    /// Apply all directory changes, stopping early on cancellation or
    /// exhausted rate limits while preserving applied work.
    async fn apply_directory_changes(
        &self,
        ctx: &Ctx,
        changes: Vec<DirectoryChange>,
        changes_applied: &mut Vec<ChangeApplied>,
    ) -> Result<()> {
        for change in changes {
            // Stop before starting another mutation when cancellation is active
            if ctx.cancel_token.is_cancelled() {
                warn!(org = %ctx.org, "stopping reconciliation early due to cancellation");
                return Err(Error::new(CancelledError));
            }

            // Dispatch the directory mutation, skipping user-level changes
            let Some(result) = self.apply_directory_change(ctx, &change).await else {
                continue;
            };

            // Record the outcome and stop early on rate-limit or cancellation errors
            let preserve_on_early_exit = matches!(change, DirectoryChange::TeamAdded(_));
            record_change_outcome(
                &ctx.org,
                Box::new(change),
                result,
                preserve_on_early_exit,
                changes_applied,
            )?;
        }

        Ok(())
    }

    /// Apply a single repository change to GitHub, returning `None` when the
    /// change is already satisfied by an earlier mutation in this run, or the
    /// result together with whether a partial mutation must be preserved on
    /// early exit.
    async fn apply_repository_change(
        &self,
        ctx: &Ctx,
        change: &RepositoryChange,
        invitations: &HashMap<RepositoryName, Vec<RepositoryInvitation>>,
        org_members: &[UserName],
        changes_applied: &[ChangeApplied],
    ) -> Option<(Result<()>, bool)> {
        let outcome = match change {
            RepositoryChange::CollaboratorAdded(repo_name, user_name, role) => {
                self.add_collaborator_or_reinvite(ctx, invitations, org_members, repo_name, user_name, role)
                    .await
            }
            RepositoryChange::CollaboratorRemoved(repo_name, user_name) => (
                self.remove_collaborator_or_invitation(ctx, invitations, repo_name, user_name).await,
                false,
            ),
            RepositoryChange::CollaboratorRoleUpdated(repo_name, user_name, role) => (
                self.update_collaborator_or_invitation_role(ctx, invitations, repo_name, user_name, role)
                    .await,
                false,
            ),
            RepositoryChange::RepositoryAdded(repo) => (self.svc.add_repository(ctx, repo).await, true),
            RepositoryChange::TeamAdded(repo_name, team_name, role) => (
                self.svc.add_repository_team(ctx, repo_name, team_name, role).await,
                false,
            ),
            RepositoryChange::TeamRemoved(repo_name, team_name) => {
                // If the team has just been successfully deleted from the
                // directory in this reconciliation, GitHub removes it from the
                // repository automatically
                if was_team_removed(changes_applied, team_name) {
                    return None;
                }
                (
                    self.svc.remove_repository_team(ctx, repo_name, team_name).await,
                    false,
                )
            }
            RepositoryChange::TeamRoleUpdated(repo_name, team_name, role) => (
                self.svc.update_repository_team_role(ctx, repo_name, team_name, role).await,
                false,
            ),
            RepositoryChange::VisibilityUpdated(repo_name, visibility) => (
                self.svc.update_repository_visibility(ctx, repo_name, visibility).await,
                false,
            ),
        };
        Some(outcome)
    }

    /// Apply all repository changes, prioritizing brand-new collaborator adds
    /// and org-member grants ahead of quota-limited expired-invitation
    /// refreshes, skipping remaining quota-limited expired refreshes for a
    /// repository once a quota-limited expired refresh returns `422
    /// Unprocessable Entity`, and stopping early on cancellation or exhausted
    /// rate limits. Only quota-limited expired refreshes arm the per-repo skip
    /// so a persistent per-user `422` on a brand-new add or org-member grant
    /// cannot permanently block unrelated expired refreshes.
    async fn apply_repository_changes(
        &self,
        ctx: &Ctx,
        mut changes: Vec<RepositoryChange>,
        invitations: &HashMap<RepositoryName, Vec<RepositoryInvitation>>,
        org_members: &[UserName],
        changes_applied: &mut Vec<ChangeApplied>,
    ) -> Result<()> {
        // Apply quota-limited expired-invitation refreshes last so brand-new
        // collaborators and org-member grants consume invitation quota first
        changes.sort_by_cached_key(|change| {
            quota_limited_expired_reinvite(change, invitations, org_members).is_some()
        });

        let mut repos_skipping_expired_reinvites = HashSet::new();
        for change in changes {
            // Stop before starting another mutation when cancellation is active
            if ctx.cancel_token.is_cancelled() {
                warn!(org = %ctx.org, "stopping reconciliation early due to cancellation");
                return Err(Error::new(CancelledError));
            }

            // Skip remaining quota-limited expired-invitation refreshes for a
            // repo after a 422 response (such as the 50 invitations per 24h
            // cap) so we do not delete invitations that cannot be recreated
            let quota_limited_refresh = quota_limited_expired_reinvite(&change, invitations, org_members);
            if let Some((repo_name, user_name)) = quota_limited_refresh
                && repos_skipping_expired_reinvites.contains(repo_name)
            {
                warn!(
                    org = %ctx.org,
                    repo = %repo_name,
                    user = %user_name,
                    "skipping remaining expired-invitation refresh after 422 response"
                );
                continue;
            }

            // Dispatch the repository mutation, skipping redundant removals
            let Some((result, preserve_on_early_exit)) = self
                .apply_repository_change(ctx, &change, invitations, org_members, changes_applied)
                .await
            else {
                continue;
            };

            // Mark this repo to skip further quota-limited expired refreshes
            // when a quota-limited expired refresh fails with 422
            if let Some((repo_name, _)) = quota_limited_refresh
                && let Err(err) = &result
                && is_unprocessable_entity_error(err)
            {
                repos_skipping_expired_reinvites.insert(repo_name.clone());
            }

            // Record the outcome and stop early on rate-limit or cancellation errors
            record_change_outcome(
                &ctx.org,
                Box::new(change),
                result,
                preserve_on_early_exit,
                changes_applied,
            )?;
        }

        Ok(())
    }

    /// Return the problems found in the users added as team members or
    /// repository collaborators by the changes provided, including the ones
    /// in new teams and repositories. Teams' maintainers are not checked here
    /// because State::validate already requires them to be organization
    /// members.
    ///
    /// Users whose exact username already appears in `org_members` are skipped
    /// without an extra `GET /users/{login}` call. Rate-limit and cancellation
    /// errors are propagated so they are never reported as invalid usernames.
    async fn collect_user_problems(
        &self,
        ctx: &Ctx,
        changes: &Changes,
        org_members: &[UserName],
    ) -> Result<Vec<Error>> {
        // Collect users to validate from changes
        let mut users_to_validate = BTreeSet::new();
        for change in &changes.directory {
            match change {
                DirectoryChange::TeamAdded(team) => users_to_validate.extend(&team.members),
                DirectoryChange::TeamMemberAdded(_, user_name) => {
                    users_to_validate.insert(user_name);
                }
                _ => {}
            }
        }
        for change in &changes.repositories {
            match change {
                RepositoryChange::RepositoryAdded(repo) => {
                    if let Some(collaborators) = &repo.collaborators {
                        users_to_validate.extend(collaborators.keys());
                    }
                }
                RepositoryChange::CollaboratorAdded(_, user_name, _) => {
                    users_to_validate.insert(user_name);
                }
                _ => {}
            }
        }

        // Validate users not already confirmed by exact-case org membership,
        // preserving typed non-404 client errors so operational outages remain
        // distinguishable from ordinary invalid-username configuration mistakes
        let mut problems = vec![];
        for user_name in users_to_validate {
            if org_members.contains(user_name) {
                continue;
            }
            match self.svc.get_user_login(ctx, user_name).await {
                Ok(valid_user_name) => {
                    if user_name != &valid_user_name {
                        problems.push(format_err!(
                            "user[{user_name}]: invalid username, should be {valid_user_name}",
                        ));
                    }
                }
                Err(err) if is_rate_limit_error(&err) || is_cancelled_error(&err) => return Err(err),
                Err(err)
                    if !is_not_found_error(&err)
                        && err.chain().any(|cause| cause.downcast_ref::<ClientError>().is_some()) =>
                {
                    problems.push(err.context(format!("user[{user_name}]: error validating username")));
                }
                Err(err) => {
                    problems.push(format_err!("user[{user_name}]: error validating username: {err}"));
                }
            }
        }
        Ok(problems)
    }

    /// Format problems returned by [`Self::collect_user_problems`] as warning
    /// strings for repair pull requests with an invalid base configuration.
    async fn find_user_problems(
        &self,
        ctx: &Ctx,
        changes: &Changes,
        org_members: &[UserName],
    ) -> Result<Vec<String>> {
        Ok(self
            .collect_user_problems(ctx, changes, org_members)
            .await?
            .into_iter()
            .map(|err| format!("{err:#}"))
            .collect())
    }

    /// Remove a pending repository invitation when present, falling back to
    /// collaborator removal if the invitation was accepted mid-reconcile.
    async fn remove_collaborator_or_invitation(
        &self,
        ctx: &Ctx,
        invitations: &HashMap<RepositoryName, Vec<RepositoryInvitation>>,
        repo_name: &RepositoryName,
        user_name: &UserName,
    ) -> Result<()> {
        if let Some(invitation_id) = find_repository_invitation(invitations, repo_name, user_name) {
            return match self.svc.remove_repository_invitation(ctx, repo_name, invitation_id).await {
                Ok(()) => Ok(()),
                Err(err) if is_not_found_error(&err) => {
                    self.svc.remove_repository_collaborator(ctx, repo_name, user_name).await
                }
                Err(err) => Err(err),
            };
        }

        self.svc.remove_repository_collaborator(ctx, repo_name, user_name).await
    }

    /// Update a pending repository invitation role when present, falling back
    /// to collaborator role update if the invitation was accepted mid-reconcile.
    async fn update_collaborator_or_invitation_role(
        &self,
        ctx: &Ctx,
        invitations: &HashMap<RepositoryName, Vec<RepositoryInvitation>>,
        repo_name: &RepositoryName,
        user_name: &UserName,
        role: &Role,
    ) -> Result<()> {
        if let Some(invitation_id) = find_repository_invitation(invitations, repo_name, user_name) {
            return match self.svc.update_repository_invitation(ctx, repo_name, invitation_id, role).await {
                Ok(()) => Ok(()),
                Err(err) if is_not_found_error(&err) => {
                    self.svc.update_repository_collaborator_role(ctx, repo_name, user_name, role).await
                }
                Err(err) => Err(err),
            };
        }

        self.svc.update_repository_collaborator_role(ctx, repo_name, user_name, role).await
    }

    /// Validate the users added by the changes provided.
    async fn validate_users(&self, ctx: &Ctx, changes: &Changes, org_members: &[UserName]) -> Result<()> {
        let mut merr = MultiError::new(Some("invalid github service configuration".to_string()));
        for problem in self.collect_user_problems(ctx, changes, org_members).await? {
            merr.push(problem);
        }

        if merr.contains_errors() {
            return Err(merr.into());
        }
        Ok(())
    }
}

#[async_trait]
impl ServiceHandler for Handler {
    /// [`ServiceHandler::get_changes_summary`].
    async fn get_changes_summary(
        &self,
        org: &Organization,
        head_src: &Source,
        job_ctx: &JobContext,
    ) -> Result<ChangesSummary> {
        // Load organization-level metadata once for both head and base states
        let ctx = Ctx::from_org_with_cancel(org, job_ctx.cancel_token.clone());
        let base_src = Source::from(org);
        let metadata = ServiceMetadata::load(&self.svc, &ctx).await?;

        // Load head and base states using the per-job cached file reader
        let head_state =
            State::new_from_config_with_metadata(job_ctx.gh.clone(), &org.legacy, &metadata, head_src)
                .await?;
        let base_state =
            match State::new_from_config_with_metadata(job_ctx.gh.clone(), &org.legacy, &metadata, &base_src)
                .await
            {
                Ok(state) => Some(state),
                Err(err) if is_rate_limit_error(&err) || is_cancelled_error(&err) => return Err(err),
                Err(_) => None,
            };

        // Get the changes between the states and check the users they add
        let (changes, base_ref_config_status, warnings) = if let Some(base_state) = base_state {
            // Diff the states and fail the validation if any user is invalid
            let changes = base_state.diff(&head_state);
            self.validate_users(&ctx, &changes, &metadata.org_members).await?;

            // Prepare the repositories changes for the summary
            let repositories_changes =
                changes.repositories.into_iter().map(|change| Box::new(change) as DynChange).collect();
            (repositories_changes, BaseRefConfigStatus::Valid, vec![])
        } else {
            // An unreadable base can't be diffed, and that's what a repair PR
            // looks like, so all the users in the head are checked but any
            // problem found doesn't fail the validation
            let changes = State::default().diff(&head_state);
            let warnings = self.find_user_problems(&ctx, &changes, &metadata.org_members).await?;
            (vec![], BaseRefConfigStatus::Invalid, warnings)
        };

        Ok(ChangesSummary {
            changes,
            base_ref_config_status,
            warnings,
        })
    }

    /// [`ServiceHandler::reconcile`].
    async fn reconcile(&self, org: &Organization, job_ctx: &JobContext) -> ReconcileOutcome {
        let ctx = Ctx::from_org_with_cancel(org, job_ctx.cancel_token.clone());
        let src = Source::from(org);

        // Load organization metadata and desired state first so invalid config
        // fails fast without scanning every team and repository
        let metadata = match ServiceMetadata::load(&self.svc, &ctx).await {
            Ok(metadata) => metadata,
            Err(err) => {
                return ReconcileOutcome::failed(
                    err.context("error loading organization metadata from service"),
                );
            }
        };
        let desired_state = match State::new_from_config_with_metadata(
            job_ctx.gh.clone(),
            &org.legacy,
            &metadata,
            &src,
        )
        .await
        {
            Ok(state) => state,
            Err(err) => {
                return ReconcileOutcome::failed(
                    err.context("error getting desired state from configuration"),
                );
            }
        };

        // Load narrowed actual state reusing org metadata and desired scope
        let snapshot = match State::new_from_service_for_desired(
            self.svc.clone(),
            &ctx,
            &desired_state,
            &metadata,
        )
        .await
        {
            Ok(snapshot) => snapshot,
            Err(err) => {
                return ReconcileOutcome::failed(err.context("error getting actual state from service"));
            }
        };
        let changes = snapshot.state.diff(&desired_state);
        debug!(?changes, "changes between the actual and the desired state");

        // Apply directory and repository changes, stopping early on
        // cancellation or exhausted rate limits while preserving applied work
        let mut changes_applied = vec![];
        match self
            .apply_changes(
                &ctx,
                changes,
                &snapshot.repository_invitations,
                &metadata.org_members,
                &mut changes_applied,
            )
            .await
        {
            Ok(()) => ReconcileOutcome::ok(changes_applied),
            Err(err) => ReconcileOutcome {
                changes_applied,
                error: Some(err),
            },
        }
    }
}

// Helpers.

/// Return an iterator over expired invitation ids matching `user_name`
/// exactly in `repo_name` from the invitations captured during actual-state
/// loading.
fn expired_repository_invitations<'a>(
    invitations_by_repo: &'a HashMap<RepositoryName, Vec<RepositoryInvitation>>,
    repo_name: &RepositoryName,
    user_name: &'a UserName,
) -> impl Iterator<Item = RepositoryInvitationId> + 'a {
    invitations_by_repo.get(repo_name).into_iter().flatten().filter_map(move |invitation| {
        if invitation.expired
            && invitation.invitee.as_ref().is_some_and(|invitee| &invitee.login == user_name)
        {
            return Some(invitation.id);
        }
        None
    })
}

/// Return the canonical GitHub login of an expired invitation in `repo_name`
/// that matches `user_name` case-insensitively with a different casing.
fn find_case_mismatched_expired_invitation<'a>(
    invitations_by_repo: &'a HashMap<RepositoryName, Vec<RepositoryInvitation>>,
    repo_name: &RepositoryName,
    user_name: &UserName,
) -> Option<&'a str> {
    invitations_by_repo.get(repo_name)?.iter().find_map(|invitation| {
        if invitation.expired
            && let Some(invitee) = &invitation.invitee
            && invitee.login.eq_ignore_ascii_case(user_name)
            && &invitee.login != user_name
        {
            return Some(invitee.login.as_str());
        }
        None
    })
}

/// Look up an active (non-expired) pending invitation id for `user_name` in
/// `repo_name` from the invitations captured during actual-state loading.
fn find_repository_invitation(
    invitations_by_repo: &HashMap<RepositoryName, Vec<RepositoryInvitation>>,
    repo_name: &RepositoryName,
    user_name: &UserName,
) -> Option<RepositoryInvitationId> {
    invitations_by_repo.get(repo_name)?.iter().find_map(|invitation| {
        if !invitation.expired
            && invitation.invitee.as_ref().is_some_and(|invitee| &invitee.login == user_name)
        {
            return Some(invitation.id);
        }
        None
    })
}

/// Return true if the error chain contains a GitHub HTTP error with the
/// status code provided.
fn is_http_status_error(err: &Error, expected: StatusCode) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<ClientError>(),
            Some(ClientError::HttpError { status, .. }) if *status == expected
        )
    })
}

/// Return true if the error chain contains a GitHub `404 Not Found` HTTP error.
fn is_not_found_error(err: &Error) -> bool {
    is_http_status_error(err, StatusCode::NOT_FOUND)
}

/// Return true if the error chain contains a GitHub `422 Unprocessable Entity`
/// HTTP error.
fn is_unprocessable_entity_error(err: &Error) -> bool {
    is_http_status_error(err, StatusCode::UNPROCESSABLE_ENTITY)
}

/// Log a single applied change as soon as it finishes so long reconciliations
/// emit progress incrementally rather than only at the end of the job.
fn log_change_applied(entry: &ChangeApplied) {
    let msg = if entry.error.is_none() {
        "change applied"
    } else {
        "something went wrong applying change"
    };
    let details = entry.change.details();
    let extra = serde_json::to_string(&details.extra).unwrap_or_default();
    debug!(
        service = SERVICE_NAME,
        kind = details.kind,
        extra,
        error = entry.error,
        "{msg}"
    );
}

/// Return `(repo_name, user_name)` when `change` refreshes an expired
/// invitation for an outside collaborator, which counts toward GitHub's
/// per-repository invitation quota.
fn quota_limited_expired_reinvite<'a>(
    change: &'a RepositoryChange,
    invitations_by_repo: &HashMap<RepositoryName, Vec<RepositoryInvitation>>,
    org_members: &[UserName],
) -> Option<(&'a RepositoryName, &'a UserName)> {
    let RepositoryChange::CollaboratorAdded(repo_name, user_name, _) = change else {
        return None;
    };
    if org_members.contains(user_name) {
        return None;
    }
    expired_repository_invitations(invitations_by_repo, repo_name, user_name)
        .next()
        .map(|_| (repo_name, user_name))
}

/// Record the outcome of applying a single change, logging it immediately and
/// returning `Err` to halt reconciliation early when rate limits or
/// cancellation are encountered.
fn record_change_outcome(
    org_name: &str,
    change: DynChange,
    result: Result<()>,
    preserve_on_early_exit: bool,
    changes_applied: &mut Vec<ChangeApplied>,
) -> Result<()> {
    match result {
        Ok(()) => {
            let entry = ChangeApplied {
                change,
                error: None,
                applied_at: time::OffsetDateTime::now_utc(),
            };
            log_change_applied(&entry);
            changes_applied.push(entry);
            Ok(())
        }
        Err(err) if is_rate_limit_error(&err) || is_cancelled_error(&err) => {
            warn!(org = %org_name, %err, "stopping reconciliation early");
            // Compound creations may have partially mutated GitHub before
            // failing; preserve an audit record for them
            if preserve_on_early_exit {
                let entry = ChangeApplied {
                    change,
                    error: Some(err.to_string()),
                    applied_at: time::OffsetDateTime::now_utc(),
                };
                log_change_applied(&entry);
                changes_applied.push(entry);
            }
            Err(err)
        }
        Err(err) => {
            let entry = ChangeApplied {
                change,
                error: Some(err.to_string()),
                applied_at: time::OffsetDateTime::now_utc(),
            };
            log_change_applied(&entry);
            changes_applied.push(entry);
            Ok(())
        }
    }
}

/// Return true when `team_name` was already removed without error earlier in
/// the same reconciliation run.
fn was_team_removed(changes_applied: &[ChangeApplied], team_name: &TeamName) -> bool {
    changes_applied.iter().any(|entry| {
        let change = (*entry.change).downcast_ref::<DirectoryChange>();
        matches!(change, Some(DirectoryChange::TeamRemoved(removed_name)) if removed_name == team_name)
            && entry.error.is_none()
    })
}
