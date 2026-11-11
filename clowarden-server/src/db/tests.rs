use std::env;

use anyhow::format_err;
use deadpool_postgres::{Config, Runtime};
use serde_json::{Value, json};
use time::{OffsetDateTime, macros::datetime};
use tokio_postgres::{NoTls, Row};

use clowarden_core::{cfg::Organization, directory::DirectoryChange, services::ChangeApplied};

use super::*;

const ALICE: &str = "alice";
const BOB: &str = "bob";
const CAROL: &str = "carol";
const DB_TESTS_URL_VAR: &str = "CLOWARDEN_DB_TESTS_URL";
const GITHUB: ServiceName = "github";
const MIDDLE_APPLIED_AT: OffsetDateTime = datetime!(2024-03-01 11:00 UTC);
const MIDDLE_APPLIED_AT_EPOCH: f64 = 1_709_290_800.0;
const MIDDLE_KIND: &str = "team-member-added";
const NEWEST_APPLIED_AT: OffsetDateTime = datetime!(2024-03-01 12:00 UTC);
const NEWEST_KIND: &str = "team-maintainer-added";
const OLDEST_APPLIED_AT: OffsetDateTime = datetime!(2024-03-01 10:00 UTC);
const OLDEST_APPLIED_AT_EPOCH: f64 = 1_709_287_200.0;
const OLDEST_ERROR: &str = "member could not be removed";
const OLDEST_KIND: &str = "team-member-removed";
const OTHER_SERVICE: ServiceName = "other-service";
const PLATFORM_TEAM: &str = "platform";
const PR_MERGED_AT: OffsetDateTime = datetime!(2024-03-01 09:30 UTC);
const PR_MERGED_AT_EPOCH: f64 = 1_709_285_400.0;
const PR_NUMBER: i64 = 101;
const SECURITY_TEAM: &str = "security";

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn db_contracts_register_reconciliation_makes_change_searchable_by_keywords() {
    // Setup database and input
    let db = setup_db();
    let input = setup_pr_input();
    let changes = HashMap::from([(GITHUB, vec![setup_member_added()])]);

    // Register the reconciliation
    db.register_reconciliation(&input, &changes, &HashMap::new()).await.unwrap();

    // Check the change is found by each pull request and change keyword
    for term in ["101", ALICE, BOB, PLATFORM_TEAM, CAROL] {
        let (count, changes) = search(&db, &setup_ts_query_web(&input.org.name, term)).await;
        assert_eq!(count, 1, "{term}");
        assert_eq!(change_kinds(&changes), [MIDDLE_KIND], "{term}");
    }

    // Check the search does not match a term that was not registered
    let (count, changes) = search(&db, &setup_ts_query_web(&input.org.name, "mallory")).await;
    assert_eq!(count, 0);
    assert_eq!(changes, Vec::<Value>::new());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn db_contracts_register_reconciliation_persists_every_column() {
    // Setup database and input
    let db = setup_db();
    let input = setup_pr_input();
    let changes = HashMap::from([(GITHUB, vec![setup_member_added()])]);

    // Register the reconciliation
    db.register_reconciliation(&input, &changes, &HashMap::new()).await.unwrap();

    // Check the reconciliation columns
    let reconciliation = fetch_reconciliation(&db, &input.org.name).await;
    assert_eq!(
        reconciliation.get::<_, Option<String>>("organization"),
        Some(input.org.name.clone())
    );
    assert_eq!(reconciliation.get::<_, Option<String>>("error"), None);
    assert_eq!(reconciliation.get::<_, Option<i64>>("pr_number"), Some(PR_NUMBER));
    assert_eq!(
        reconciliation.get::<_, Option<String>>("pr_created_by"),
        Some(ALICE.to_string())
    );
    assert_eq!(
        reconciliation.get::<_, Option<String>>("pr_merged_by"),
        Some(BOB.to_string())
    );
    assert_eq!(
        reconciliation.get::<_, Option<OffsetDateTime>>("pr_merged_at"),
        Some(PR_MERGED_AT)
    );

    // Check the change columns
    let changes = fetch_changes(&db, &input.org.name).await;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].get::<_, String>("service"), GITHUB);
    assert_eq!(changes[0].get::<_, String>("kind"), MIDDLE_KIND);
    assert_eq!(
        changes[0].get::<_, Option<Value>>("extra"),
        Some(json!({ "team_name": PLATFORM_TEAM, "user_name": CAROL }))
    );
    assert_eq!(
        changes[0].get::<_, OffsetDateTime>("applied_at"),
        MIDDLE_APPLIED_AT
    );
    assert_eq!(changes[0].get::<_, Option<String>>("error"), None);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn db_contracts_register_reconciliation_round_trips_through_search_changes() {
    // Setup database and input
    let db = setup_db();
    let input = setup_pr_input();
    let changes = HashMap::from([(GITHUB, vec![setup_member_added(), setup_member_removed()])]);
    let errors = HashMap::from([(GITHUB, format_err!("github is down"))]);

    // Register the reconciliation
    db.register_reconciliation(&input, &changes, &errors).await.unwrap();

    // Read the changes back, oldest first
    let (count, changes) = search(
        &db,
        &SearchChangesInput {
            sort_direction: Some("asc".to_string()),
            ..setup_org_search(&input.org.name)
        },
    )
    .await;
    assert_eq!(count, 2);
    let [failed, successful] = changes.as_slice() else {
        panic!("expected two changes, got {changes:?}");
    };

    // Check the failed change
    assert!(Uuid::parse_str(failed["change_id"].as_str().unwrap()).is_ok());
    assert_eq!(failed["service"], GITHUB);
    assert_eq!(failed["kind"], OLDEST_KIND);
    assert_eq!(
        failed["extra"],
        json!({ "team_name": SECURITY_TEAM, "user_name": CAROL })
    );
    assert_eq!(failed["applied_at"].as_f64(), Some(OLDEST_APPLIED_AT_EPOCH));
    assert_eq!(failed["error"], OLDEST_ERROR);

    // Check the successful change
    assert!(Uuid::parse_str(successful["change_id"].as_str().unwrap()).is_ok());
    assert_ne!(failed["change_id"], successful["change_id"]);
    assert_eq!(successful["service"], GITHUB);
    assert_eq!(successful["kind"], MIDDLE_KIND);
    assert_eq!(
        successful["extra"],
        json!({ "team_name": PLATFORM_TEAM, "user_name": CAROL })
    );
    assert_eq!(successful["applied_at"].as_f64(), Some(MIDDLE_APPLIED_AT_EPOCH));
    assert!(successful.get("error").is_none());

    // Check both changes belong to the reconciliation registered
    let row = fetch_reconciliation(&db, &input.org.name).await;
    let reconciliation = &failed["reconciliation"];
    assert_eq!(*reconciliation, successful["reconciliation"]);
    assert_eq!(
        reconciliation["reconciliation_id"],
        row.get::<_, Uuid>("reconciliation_id").to_string().as_str()
    );
    assert_eq!(reconciliation["organization"], input.org.name.as_str());
    assert!(reconciliation["completed_at"].is_number());
    assert_eq!(
        reconciliation["error"],
        format!("{GITHUB}: {:?}", errors[GITHUB]).as_str()
    );
    assert_eq!(reconciliation["pr_number"], PR_NUMBER);
    assert_eq!(reconciliation["pr_created_by"], ALICE);
    assert_eq!(reconciliation["pr_merged_by"], BOB);
    assert_eq!(reconciliation["pr_merged_at"].as_f64(), Some(PR_MERGED_AT_EPOCH));
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn db_contracts_register_reconciliation_stores_nulls_for_scheduled_run() {
    // Setup database and input
    let db = setup_db();
    let input = setup_scheduled_input();
    let changes = HashMap::from([(GITHUB, vec![setup_member_added(), setup_member_removed()])]);

    // Register the reconciliation
    db.register_reconciliation(&input, &changes, &HashMap::new()).await.unwrap();

    // Check the reconciliation columns are null
    let reconciliation = fetch_reconciliation(&db, &input.org.name).await;
    assert_eq!(reconciliation.get::<_, Option<String>>("error"), None);
    assert_eq!(reconciliation.get::<_, Option<i64>>("pr_number"), None);
    assert_eq!(reconciliation.get::<_, Option<String>>("pr_created_by"), None);
    assert_eq!(reconciliation.get::<_, Option<String>>("pr_merged_by"), None);
    assert_eq!(
        reconciliation.get::<_, Option<OffsetDateTime>>("pr_merged_at"),
        None
    );

    // Check only the failed change stores an error
    let changes = fetch_changes(&db, &input.org.name).await;
    assert_eq!(changes.len(), 2);
    assert_eq!(
        changes[0].get::<_, Option<String>>("error"),
        Some(OLDEST_ERROR.to_string())
    );
    assert_eq!(changes[1].get::<_, Option<String>>("error"), None);

    // Check the change is found by its own keywords and omits the null fields
    let (count, changes) = search(&db, &setup_ts_query_web(&input.org.name, PLATFORM_TEAM)).await;
    assert_eq!(count, 1);
    assert_eq!(change_kinds(&changes), [MIDDLE_KIND]);
    for field in [
        "error",
        "pr_number",
        "pr_created_by",
        "pr_merged_by",
        "pr_merged_at",
    ] {
        assert!(changes[0]["reconciliation"].get(field).is_none(), "{field}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn db_contracts_register_reconciliation_stores_summary_for_errors_without_changes() {
    // Setup database and input
    let db = setup_db();
    let input = setup_scheduled_input();
    let errors = HashMap::from([
        (GITHUB, format_err!("github is down")),
        (OTHER_SERVICE, format_err!("other service is down")),
    ]);

    // Register the reconciliation
    db.register_reconciliation(&input, &HashMap::new(), &errors).await.unwrap();

    // Check the summary has one line per service. The expected lines are built
    // from the errors because their debug format includes any backtrace captured
    // when RUST_BACKTRACE is set, as it is in CI, and services can come in any order
    let github_line = format!("{GITHUB}: {:?}", errors[GITHUB]);
    let other_service_line = format!("{OTHER_SERVICE}: {:?}", errors[OTHER_SERVICE]);
    let reconciliation = fetch_reconciliation(&db, &input.org.name).await;
    let summary = reconciliation.get::<_, Option<String>>("error").unwrap();
    assert!(
        summary == format!("{github_line}\n{other_service_line}")
            || summary == format!("{other_service_line}\n{github_line}"),
        "unexpected summary: {summary}"
    );

    // Check the pull request columns are null
    assert_eq!(reconciliation.get::<_, Option<i64>>("pr_number"), None);
    assert_eq!(reconciliation.get::<_, Option<String>>("pr_created_by"), None);
    assert_eq!(reconciliation.get::<_, Option<String>>("pr_merged_by"), None);
    assert_eq!(
        reconciliation.get::<_, Option<OffsetDateTime>>("pr_merged_at"),
        None
    );

    // Check no changes were stored
    assert!(fetch_changes(&db, &input.org.name).await.is_empty());
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn db_contracts_search_changes_applies_each_input_field() {
    // Setup database and seed data
    let db = setup_db();
    let org = setup_org_name();
    let other_org = setup_org_name();
    register_search_seed(&db, &org).await;
    register_other_org_seed(&db, &other_org).await;

    // Setup one input per field, each differing from the org's unfiltered results
    let org_input = setup_org_search(&org);
    let with = |update: fn(&mut SearchChangesInput)| {
        let mut input = org_input.clone();
        update(&mut input);
        input
    };
    let newest_first = vec![NEWEST_KIND, MIDDLE_KIND, OLDEST_KIND];
    let oldest_first = vec![OLDEST_KIND, MIDDLE_KIND, NEWEST_KIND];
    let cases = [
        ("none", org_input.clone(), 3, newest_first),
        ("limit", with(|i| i.limit = Some(1)), 3, vec![NEWEST_KIND]),
        (
            "offset",
            with(|i| i.offset = Some(1)),
            3,
            vec![MIDDLE_KIND, OLDEST_KIND],
        ),
        (
            "sort_direction",
            with(|i| i.sort_direction = Some("asc".to_string())),
            3,
            oldest_first,
        ),
        (
            "organization",
            setup_org_search(&other_org),
            1,
            vec!["team-removed"],
        ),
        (
            "service",
            with(|i| i.service = Some(vec![OTHER_SERVICE.to_string()])),
            1,
            vec![NEWEST_KIND],
        ),
        (
            "kind",
            with(|i| i.kind = Some(vec![OLDEST_KIND.to_string()])),
            1,
            vec![OLDEST_KIND],
        ),
        (
            "applied_from",
            with(|i| i.applied_from = Some("2024-03-01T10:30:00Z".to_string())),
            2,
            vec![NEWEST_KIND, MIDDLE_KIND],
        ),
        (
            "applied_to",
            with(|i| i.applied_to = Some("2024-03-01T10:30:00Z".to_string())),
            1,
            vec![OLDEST_KIND],
        ),
        (
            "pr_number",
            with(|i| i.pr_number = Some(vec![PR_NUMBER + 1])),
            1,
            vec![NEWEST_KIND],
        ),
        (
            "pr_merged_by",
            with(|i| i.pr_merged_by = Some(vec![CAROL.to_string()])),
            1,
            vec![NEWEST_KIND],
        ),
        (
            "applied_successfully",
            with(|i| i.applied_successfully = Some(true)),
            2,
            vec![NEWEST_KIND, MIDDLE_KIND],
        ),
        (
            "ts_query_web",
            setup_ts_query_web(&org, SECURITY_TEAM),
            1,
            vec![OLDEST_KIND],
        ),
    ];

    // Check each field takes effect
    for (field, input, expected_count, expected_kinds) in cases {
        let (count, changes) = search(&db, &input).await;
        assert_eq!(count, expected_count, "{field}");
        assert_eq!(change_kinds(&changes), expected_kinds, "{field}");
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn db_contracts_search_changes_sorts_by_date_and_ignores_other_sort_by_values() {
    // Setup database and seed data out of date order
    let db = setup_db();
    let input = setup_scheduled_input();
    let changes = HashMap::from([(
        GITHUB,
        vec![
            setup_member_added(),
            setup_member_removed(),
            setup_maintainer_added(),
        ],
    )]);
    db.register_reconciliation(&input, &changes, &HashMap::new()).await.unwrap();

    // Setup the search inputs
    let sort_by_date = SearchChangesInput {
        sort_by: Some("date".to_string()),
        sort_direction: Some("asc".to_string()),
        ..setup_org_search(&input.org.name)
    };
    let sort_by_other = SearchChangesInput {
        sort_by: Some("other".to_string()),
        ..sort_by_date.clone()
    };

    // Check date sorts the changes
    let (_, changes) = search(&db, &sort_by_date).await;
    assert_eq!(change_kinds(&changes), [OLDEST_KIND, MIDDLE_KIND, NEWEST_KIND]);

    // Check other values return all the changes, whose order is undefined
    let (count, changes) = search(&db, &sort_by_other).await;
    let mut kinds = change_kinds(&changes);
    kinds.sort_unstable();
    assert_eq!(count, 3);
    assert_eq!(kinds, [NEWEST_KIND, MIDDLE_KIND, OLDEST_KIND]);
}

// Helpers.

/// Return the kinds of the changes provided.
fn change_kinds(changes: &[Value]) -> Vec<&str> {
    changes.iter().map(|change| change["kind"].as_str().unwrap()).collect()
}

/// Fetch the changes of the organization provided, oldest first.
async fn fetch_changes(db: &PgDB, org: &str) -> Vec<Row> {
    let client = db.pool.get().await.unwrap();
    client
        .query(
            "
            select c.service, c.kind, c.extra, c.applied_at, c.error
            from change c
            join reconciliation r using (reconciliation_id)
            where r.organization = $1::text
            order by c.applied_at asc
            ",
            &[&org],
        )
        .await
        .unwrap()
}

/// Fetch the only reconciliation of the organization provided.
async fn fetch_reconciliation(db: &PgDB, org: &str) -> Row {
    let client = db.pool.get().await.unwrap();
    client
        .query_one(
            "select * from reconciliation where organization = $1::text",
            &[&org],
        )
        .await
        .unwrap()
}

/// Register the only change of the other organization used in search scenarios.
async fn register_other_org_seed(db: &PgDB, org: &str) {
    let input = ReconcileInput {
        org: setup_org(org),
        ..Default::default()
    };
    let changes = HashMap::from([(
        GITHUB,
        vec![ChangeApplied {
            change: Box::new(DirectoryChange::TeamRemoved(PLATFORM_TEAM.to_string())),
            error: None,
            applied_at: OLDEST_APPLIED_AT,
        }],
    )]);
    db.register_reconciliation(&input, &changes, &HashMap::new()).await.unwrap();
}

/// Register two reconciliations with changes that differ in every search field.
async fn register_search_seed(db: &PgDB, org: &str) {
    // First pull request, with the middle and oldest changes
    let input = ReconcileInput {
        org: setup_org(org),
        pr_number: Some(PR_NUMBER),
        pr_created_by: Some(ALICE.to_string()),
        pr_merged_by: Some(BOB.to_string()),
        pr_merged_at: Some(PR_MERGED_AT),
    };
    let changes = HashMap::from([(GITHUB, vec![setup_member_added(), setup_member_removed()])]);
    db.register_reconciliation(&input, &changes, &HashMap::new()).await.unwrap();

    // Second pull request, with the newest change
    let input = ReconcileInput {
        org: setup_org(org),
        pr_number: Some(PR_NUMBER + 1),
        pr_created_by: Some(ALICE.to_string()),
        pr_merged_by: Some(CAROL.to_string()),
        pr_merged_at: Some(PR_MERGED_AT),
    };
    let changes = HashMap::from([(OTHER_SERVICE, vec![setup_maintainer_added()])]);
    db.register_reconciliation(&input, &changes, &HashMap::new()).await.unwrap();
}

/// Run a search and parse the changes returned.
async fn search(db: &PgDB, input: &SearchChangesInput) -> (Count, Vec<Value>) {
    let (count, changes) = db.search_changes(input).await.unwrap();
    (count, serde_json::from_str(&changes).unwrap())
}

/// Create a PgDB instance for the database in the tests URL variable.
fn setup_db() -> PgDB {
    let mut cfg = Config::new();
    cfg.url = Some(env::var(DB_TESTS_URL_VAR).unwrap_or_else(|_| panic!("{DB_TESTS_URL_VAR} must be set")));
    PgDB::new(cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap())
}

/// Create the newest change, a successful one.
fn setup_maintainer_added() -> ChangeApplied {
    ChangeApplied {
        change: Box::new(DirectoryChange::TeamMaintainerAdded(
            "storage".to_string(),
            CAROL.to_string(),
        )),
        error: None,
        applied_at: NEWEST_APPLIED_AT,
    }
}

/// Create the middle change, a successful one.
fn setup_member_added() -> ChangeApplied {
    ChangeApplied {
        change: Box::new(DirectoryChange::TeamMemberAdded(
            PLATFORM_TEAM.to_string(),
            CAROL.to_string(),
        )),
        error: None,
        applied_at: MIDDLE_APPLIED_AT,
    }
}

/// Create the oldest change, a failed one.
fn setup_member_removed() -> ChangeApplied {
    ChangeApplied {
        change: Box::new(DirectoryChange::TeamMemberRemoved(
            SECURITY_TEAM.to_string(),
            CAROL.to_string(),
        )),
        error: Some(OLDEST_ERROR.to_string()),
        applied_at: OLDEST_APPLIED_AT,
    }
}

/// Create an organization with the name provided.
fn setup_org(name: &str) -> Organization {
    Organization {
        name: name.to_string(),
        ..Default::default()
    }
}

/// Create a unique organization name. Tests run in parallel against a database
/// that keeps the rows they commit, so each one works on its own organization.
fn setup_org_name() -> String {
    format!("db-contracts-{}", Uuid::new_v4())
}

/// Create a search input limited to the organization provided.
fn setup_org_search(org: &str) -> SearchChangesInput {
    SearchChangesInput {
        organization: Some(org.to_string()),
        ..Default::default()
    }
}

/// Create a reconcile input for a merged pull request in a unique organization.
fn setup_pr_input() -> ReconcileInput {
    ReconcileInput {
        org: setup_org(&setup_org_name()),
        pr_number: Some(PR_NUMBER),
        pr_created_by: Some(ALICE.to_string()),
        pr_merged_by: Some(BOB.to_string()),
        pr_merged_at: Some(PR_MERGED_AT),
    }
}

/// Create a reconcile input for a scheduled run in a unique organization.
fn setup_scheduled_input() -> ReconcileInput {
    ReconcileInput {
        org: setup_org(&setup_org_name()),
        ..Default::default()
    }
}

/// Create a full text search input limited to the organization provided.
fn setup_ts_query_web(org: &str, term: &str) -> SearchChangesInput {
    SearchChangesInput {
        ts_query_web: Some(term.to_string()),
        ..setup_org_search(org)
    }
}
