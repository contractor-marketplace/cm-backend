//! Migration execution and status.
//!
//! Migrations are embedded in the binary at compile time, so the deployed
//! artefact and the schema it expects can never drift apart: there is no
//! separate directory to forget to ship.

use cm_core::AppError;
use sqlx::migrate::Migrator;
use sqlx::PgPool;

/// Every migration in `migrations/`, embedded at compile time.
pub static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

/// The highest migration version this binary carries.
pub fn embedded_version() -> i64 {
    MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0)
}

/// Where the database stands relative to this binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStatus {
    /// Highest successfully applied version, or `None` on a database that has
    /// never been migrated.
    pub applied: Option<i64>,
    /// Highest version this binary carries.
    pub embedded: i64,
    /// Versions recorded as started but not completed. A dirty migration means
    /// a human has to look, so it is never treated as ready.
    pub dirty: Vec<i64>,
}

impl MigrationStatus {
    pub fn is_up_to_date(&self) -> bool {
        self.dirty.is_empty() && self.applied == Some(self.embedded)
    }

    /// Why this schema cannot be relied on, in words an operator can act on.
    ///
    /// One function, two callers: `/readyz` reports it, and `serve` refuses to
    /// start on it. Sharing the rule is what stops a process from serving
    /// traffic while advertising itself as not ready.
    pub fn blocking_reason(&self) -> Option<String> {
        if !self.dirty.is_empty() {
            return Some(format!(
                "migration(s) {:?} are recorded as incomplete; the database needs manual inspection",
                self.dirty
            ));
        }
        match self.applied {
            None => Some(format!(
                "no migrations have been applied; this binary expects version {}",
                self.embedded
            )),
            Some(applied) if applied < self.embedded => Some(format!(
                "database is at migration {applied}, this binary expects {}",
                self.embedded
            )),
            // A database ahead of the binary is the normal middle of a rollout:
            // migrations are applied first, then the new binary starts. Since
            // every migration is additive, the old binary keeps working, so
            // this is explicitly not a readiness failure.
            Some(_) => None,
        }
    }
}

/// Apply everything outstanding. Idempotent: already-applied versions are
/// skipped, and re-running against an up-to-date database is a no-op.
pub async fn run(pool: &PgPool) -> Result<(), AppError> {
    MIGRATOR.run(pool).await.map_err(AppError::internal)
}

/// Read migration state without applying anything.
pub async fn status(pool: &PgPool) -> Result<MigrationStatus, AppError> {
    let embedded = embedded_version();

    // A database that has never been migrated has no _sqlx_migrations table,
    // and querying it would be an error rather than an empty result.
    let table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
            .fetch_one(pool)
            .await
            .map_err(AppError::internal)?;

    if !table_exists {
        return Ok(MigrationStatus {
            applied: None,
            embedded,
            dirty: Vec::new(),
        });
    }

    let applied: Option<i64> =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(pool)
            .await
            .map_err(AppError::internal)?;

    let dirty: Vec<i64> = sqlx::query_scalar(
        "SELECT version FROM _sqlx_migrations WHERE NOT success ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .map_err(AppError::internal)?;

    Ok(MigrationStatus {
        applied,
        embedded,
        dirty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binary_carries_the_expected_migrations() {
        let versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert_eq!(
            versions,
            (1..=20).collect::<Vec<i64>>(),
            "extensions and reference data, then accounts, credentials, sessions, \
             audit, rate limits, federated identity, the licence register, \
             contractors, geocoding, claims, profiles, messaging, safety, \
             the homeowner/contractor account split, jobs, the structured job \
             intake with photos, publishing the licence address, and the \
             contractor data-source marker"
        );
        assert_eq!(embedded_version(), 20);
    }

    #[test]
    fn migrations_are_forward_only() {
        for migration in MIGRATOR.iter() {
            assert!(
                matches!(
                    migration.migration_type,
                    sqlx::migrate::MigrationType::Simple
                ),
                "migration {} is reversible; every migration must be forward-only",
                migration.version
            );
        }
    }

    #[test]
    fn a_database_ahead_of_the_binary_is_still_ready() {
        // The normal middle of a rollout: migrations applied, old binary still
        // serving. Additive migrations make this safe by construction.
        let status = MigrationStatus {
            applied: Some(5),
            embedded: 2,
            dirty: Vec::new(),
        };
        assert_eq!(status.blocking_reason(), None);
    }

    #[test]
    fn a_database_behind_the_binary_blocks_readiness() {
        let status = MigrationStatus {
            applied: Some(1),
            embedded: 2,
            dirty: Vec::new(),
        };
        let reason = status.blocking_reason().expect("should block");
        assert!(reason.contains('1') && reason.contains('2'), "{reason}");
        assert!(!status.is_up_to_date());
    }

    #[test]
    fn a_dirty_migration_blocks_readiness_even_when_the_version_matches() {
        let status = MigrationStatus {
            applied: Some(2),
            embedded: 2,
            dirty: vec![2],
        };
        assert!(!status.is_up_to_date());
        assert!(status
            .blocking_reason()
            .expect("should block")
            .contains("incomplete"));
    }

    #[test]
    fn an_unmigrated_database_blocks_readiness() {
        let status = MigrationStatus {
            applied: None,
            embedded: 2,
            dirty: Vec::new(),
        };
        assert!(status
            .blocking_reason()
            .expect("should block")
            .contains("no migrations"));
    }
}
