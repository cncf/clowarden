use anyhow::Result;
use async_trait::async_trait;
use deadpool_postgres::Pool;
#[cfg(test)]
use mockall::automock;
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

use crate::{
    jobs::ReconcileInput,
    services::{ChangesApplied, ServiceName},
};

/// Trait that defines some operations a DB implementation must support.
#[async_trait]
#[cfg_attr(test, automock)]
pub(crate) trait DB {
    /// Register the reconciliation provided.
    async fn register_reconciliation(
        &self,
        input: &ReconcileInput,
        changes_applied: &HashMap<ServiceName, ChangesApplied>,
    ) -> Result<()>;
}

/// Type alias to represent a DB trait object.
pub(crate) type DynDB = Arc<dyn DB + Send + Sync>;

/// Type alias to represent a reconciliation id.
pub(crate) type ReconciliationId = Uuid;

/// DB implementation backed by PostgreSQL.
pub(crate) struct PgDB {
    pool: Pool,
}

impl PgDB {
    /// Create a new PgDB instance.
    pub(crate) fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl DB for PgDB {
    /// [DB::register_reconciliation]
    async fn register_reconciliation(
        &self,
        input: &ReconcileInput,
        changes_applied: &HashMap<ServiceName, ChangesApplied>,
    ) -> Result<()> {
        let mut db = self.pool.get().await?;
        let tx = db.transaction().await?;

        // Register reconciliation entry
        let reconciliation_id: ReconciliationId = tx
            .query_one(
                "
                insert into reconciliation (
                    error,
                    pr_number,
                    pr_created_by,
                    pr_merged_by,
                    pr_merged_at
                ) values (
                    $1::text,
                    $2::bigint,
                    $3::text,
                    $4::text,
                    $5::timestamptz
                )
                returning reconciliation_id
                ",
                &[
                    &input.error,
                    &input.pr_number,
                    &input.pr_created_by,
                    &input.pr_merged_by,
                    &input.pr_merged_at,
                ],
            )
            .await?
            .get("reconciliation_id");

        // Register changes
        for (service_name, service_changes_applied) in changes_applied {
            for change_applied in service_changes_applied {
                let change_details = change_applied.change.details();
                tx.execute(
                    "
                insert into change (
                    service,
                    kind,
                    extra,
                    applied_at,
                    error,
                    reconciliation_id
                ) values (
                    $1::text,
                    $2::text,
                    $3::jsonb,
                    $4::timestamptz,
                    $5::text,
                    $6::uuid
                )
                ",
                    &[
                        service_name,
                        &change_details.kind,
                        &change_details.extra,
                        &change_applied.applied_at,
                        &change_applied.error,
                        &reconciliation_id,
                    ],
                )
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }
}
