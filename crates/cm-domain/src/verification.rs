//! The single authority for the verified badge.
//!
//! Nothing else in the codebase writes `contractors.verified`. A request body
//! that mentions the field is rejected outright rather than ignored — silently
//! ignoring it teaches a client that it worked.
//!
//! A badge is a claim about the world, and the world moves. The reason and the
//! import it came from are stored with it, so "why is this contractor verified"
//! is answerable later by someone who was not there.

use chrono::Utc;
use cm_core::{new_id, AppError};
use cm_db::repo::contractors;
use cm_db::repo::licenses::{self, LicenseStatus};
use sqlx::PgConnection;
use uuid::Uuid;

/// What the badge currently rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub verified: bool,
    pub reason: String,
}

/// Recompute and store the badge for one contractor.
///
/// Called from exactly three places: a claim decision, an import, and the
/// nightly re-check. Every one of them passes a transaction, so the badge and
/// the evidence for it commit together.
pub async fn recompute(
    conn: &mut PgConnection,
    contractor_id: Uuid,
    source_run_id: Option<Uuid>,
) -> Result<Outcome, AppError> {
    let claimed = contractors::location_inputs(conn, contractor_id)
        .await?
        .map(|inputs| inputs.is_claimed)
        .unwrap_or(false);

    let facts = licenses::facts_for_contractor(conn, contractor_id).await?;

    let outcome = decide(claimed, facts.as_ref());

    // The observation is recorded whether or not it changed anything, so the
    // history shows what was true at each import rather than only the changes.
    if let Some(facts) = &facts {
        let passed = facts.status == LicenseStatus::Active && !is_expired(facts);
        sqlx::query(
            "INSERT INTO verification_checks \
                 (id, contractor_id, kind, outcome, evidence, source_run_id, observed_at) \
             VALUES ($1, $2, 'cslb_license_active', $3, $4, $5, now())",
        )
        .bind(new_id())
        .bind(contractor_id)
        .bind(if passed { "pass" } else { "fail" })
        .bind(serde_json::json!({
            "license_no": facts.license_no,
            "status": facts.status.as_str(),
            "expiration_date": facts.expiration_date,
        }))
        .bind(source_run_id)
        .execute(&mut *conn)
        .await
        .map_err(AppError::internal)?;
    }

    contractors::set_verification(conn, contractor_id, outcome.verified, &outcome.reason).await?;

    Ok(outcome)
}

fn is_expired(facts: &licenses::LicenseFacts) -> bool {
    facts
        .expiration_date
        .is_some_and(|date| date < Utc::now().date_naive())
}

/// The rule itself, kept pure so it can be tested without a database.
///
/// Both halves are required. A licence alone is a fact about a business, not
/// about the person holding this account; a claim alone is an assertion nobody
/// has checked.
pub fn decide(claimed: bool, facts: Option<&licenses::LicenseFacts>) -> Outcome {
    let Some(facts) = facts else {
        return Outcome {
            verified: false,
            reason: "no licence record is linked to this listing".to_owned(),
        };
    };

    if facts.status != LicenseStatus::Active {
        return Outcome {
            verified: false,
            reason: format!(
                "CSLB licence {} is {} as of the last import",
                facts.license_no,
                facts.status.as_str()
            ),
        };
    }

    if is_expired(facts) {
        return Outcome {
            verified: false,
            reason: format!(
                "CSLB licence {} expired on {}",
                facts.license_no,
                facts
                    .expiration_date
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "an unknown date".to_owned())
            ),
        };
    }

    if !claimed {
        return Outcome {
            verified: false,
            reason: format!(
                "CSLB licence {} is active, but nobody has claimed this listing",
                facts.license_no
            ),
        };
    }

    Outcome {
        verified: true,
        reason: format!(
            "claim approved and CSLB licence {} was active at the last import",
            facts.license_no
        ),
    }
}

/// Re-check every contractor. Run nightly, and after an import.
///
/// Paged rather than loaded at once: the whole table does not belong in memory
/// on a box this size.
pub async fn recompute_all(pool: &cm_db::PgPool, page: i64) -> Result<u64, AppError> {
    let mut processed = 0;
    let mut after: Option<Uuid> = None;

    loop {
        let ids: Vec<Uuid> = {
            let mut conn = pool.acquire().await.map_err(AppError::internal)?;
            sqlx::query_scalar(
                "SELECT id FROM contractors WHERE ($1::uuid IS NULL OR id > $1) \
                 ORDER BY id LIMIT $2",
            )
            .bind(after)
            .bind(page)
            .fetch_all(&mut *conn)
            .await
            .map_err(AppError::internal)?
        };

        if ids.is_empty() {
            break;
        }
        after = ids.last().copied();

        let mut tx = pool.begin().await.map_err(AppError::internal)?;
        for id in &ids {
            recompute(&mut tx, *id, None).await?;
        }
        tx.commit().await.map_err(AppError::internal)?;
        processed += ids.len() as u64;
    }

    Ok(processed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn facts(status: LicenseStatus, expires: Option<NaiveDate>) -> licenses::LicenseFacts {
        licenses::LicenseFacts {
            license_no: "1047382".to_owned(),
            status,
            expiration_date: expires,
            last_seen_at: Utc::now(),
        }
    }

    #[test]
    fn both_halves_are_required() {
        let active = facts(LicenseStatus::Active, None);

        assert!(decide(true, Some(&active)).verified);
        assert!(
            !decide(false, Some(&active)).verified,
            "an unclaimed listing is never verified, however good its licence"
        );
        assert!(
            !decide(true, None).verified,
            "a claim over no licence record verifies nothing"
        );
    }

    #[test]
    fn a_licence_that_is_not_active_removes_the_badge() {
        for status in [
            LicenseStatus::Expired,
            LicenseStatus::Suspended,
            LicenseStatus::Inactive,
            LicenseStatus::Unknown,
        ] {
            let outcome = decide(true, Some(&facts(status, None)));
            assert!(!outcome.verified, "{status:?}");
            assert!(outcome.reason.contains("1047382"), "{}", outcome.reason);
        }
    }

    #[test]
    fn an_expired_date_removes_the_badge_even_when_the_status_says_active() {
        let yesterday = Utc::now().date_naive() - chrono::Duration::days(1);
        let outcome = decide(true, Some(&facts(LicenseStatus::Active, Some(yesterday))));

        assert!(!outcome.verified);
        assert!(outcome.reason.contains("expired"), "{}", outcome.reason);
    }

    #[test]
    fn a_future_expiry_is_fine() {
        let next_year = Utc::now().date_naive() + chrono::Duration::days(365);
        assert!(decide(true, Some(&facts(LicenseStatus::Active, Some(next_year)))).verified);
    }

    #[test]
    fn every_reason_says_something_actionable() {
        let outcomes = [
            decide(false, None),
            decide(true, None),
            decide(false, Some(&facts(LicenseStatus::Active, None))),
            decide(true, Some(&facts(LicenseStatus::Suspended, None))),
        ];
        for outcome in outcomes {
            assert!(!outcome.reason.is_empty());
            assert!(outcome.reason.len() <= 500, "the column bounds this");
        }
    }
}
