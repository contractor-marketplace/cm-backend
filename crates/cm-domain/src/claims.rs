//! The claim workflow.
//!
//! A claim is an assertion that an account belongs to a business on the CSLB
//! register. Approving one is the only way a listing becomes editable, and —
//! together with an active licence — the only way it becomes verified.
//!
//! Nothing here trusts the claimant's own evidence on its own. What they submit
//! is stored as an assertion; what makes the decision is a typed
//! `verification_checks` row written by whoever or whatever actually checked.

use cm_core::AppError;
use cm_db::repo::audit::{ActorKind, AuditEvent};
use cm_db::repo::claims::{self, Claim, ClaimMethod, ClaimStatus};
use cm_db::repo::{audit, contractors, users};
use cm_db::PgPool;
use uuid::Uuid;

/// Open a claim on a listing.
pub async fn open(
    pool: &PgPool,
    contractor_id: Uuid,
    user_id: Uuid,
    method: ClaimMethod,
    evidence: serde_json::Value,
    request_id: Option<String>,
) -> Result<Claim, AppError> {
    let mut tx = pool.begin().await.map_err(AppError::internal)?;

    // A listing that is already claimed is not claimable, and saying so is
    // fine: whether a public listing has an owner is itself public.
    let target = contractors::messaging_target(&mut tx, contractor_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if target.claimed_by_user_id.is_some() {
        return Err(AppError::conflict("This listing has already been claimed."));
    }
    if contractors::claimed_by(&mut tx, user_id).await?.is_some() {
        return Err(AppError::conflict(
            "Your account has already claimed a listing.",
        ));
    }

    let claim = claims::open(&mut tx, contractor_id, user_id, method, &evidence).await?;

    audit::record(
        &mut tx,
        AuditEvent::new("claim.opened", "contractor_claims")
            .actor(ActorKind::User, Some(user_id))
            .subject(claim.id)
            .data(serde_json::json!({
                "contractor_id": contractor_id,
                "method": method.as_str(),
            }))
            .request_id(request_id),
    )
    .await?;

    tx.commit().await.map_err(AppError::internal)?;
    Ok(claim)
}

/// Withdraw one's own pending claim.
pub async fn withdraw(
    pool: &PgPool,
    claim_id: Uuid,
    user_id: Uuid,
    request_id: Option<String>,
) -> Result<(), AppError> {
    let mut tx = pool.begin().await.map_err(AppError::internal)?;

    let claim = claims::find(&mut tx, claim_id)
        .await?
        .ok_or(AppError::NotFound)?;
    // 404 rather than 403: someone else's claim is not theirs to know about.
    if claim.user_id != user_id {
        return Err(AppError::NotFound);
    }
    if claim.status != ClaimStatus::Pending {
        return Err(AppError::conflict("That claim has already been decided."));
    }

    claims::decide(
        &mut tx,
        claim_id,
        ClaimStatus::Withdrawn,
        Some(user_id),
        None,
    )
    .await?;
    audit::record(
        &mut tx,
        AuditEvent::new("claim.withdrawn", "contractor_claims")
            .actor(ActorKind::User, Some(user_id))
            .subject(claim_id)
            .request_id(request_id),
    )
    .await?;

    tx.commit().await.map_err(AppError::internal)?;
    Ok(())
}

/// The outcome of an administrative decision.
#[derive(Debug, Clone)]
pub struct Decision {
    pub claim: Claim,
    pub verified: bool,
    pub verification_reason: String,
}

/// Approve or reject a claim.
///
/// Everything commits together: the decision, the ownership link, the evidence
/// row, the recomputed badge and the audit entry. A partial application of this
/// would leave a listing owned but unverified, or verified but unowned.
pub async fn decide(
    pool: &PgPool,
    claim_id: Uuid,
    approve: bool,
    admin_id: Uuid,
    note: Option<String>,
    request_id: Option<String>,
) -> Result<Decision, AppError> {
    let mut tx = pool.begin().await.map_err(AppError::internal)?;

    let claim = claims::find(&mut tx, claim_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if claim.status != ClaimStatus::Pending {
        return Err(AppError::conflict("That claim has already been decided."));
    }

    let status = if approve {
        ClaimStatus::Approved
    } else {
        ClaimStatus::Rejected
    };

    // The guard on `pending` is what makes two simultaneous approvals safe:
    // the second changes no rows and is told so.
    if !claims::decide(&mut tx, claim_id, status, Some(admin_id), note.as_deref()).await? {
        return Err(AppError::conflict("That claim has already been decided."));
    }

    if approve {
        if !contractors::attach_claimant(&mut tx, claim.contractor_id, claim.user_id).await? {
            return Err(AppError::conflict(
                "That listing was claimed by someone else.",
            ));
        }
        // The claimant can manage the listing, so the role follows the claim.
        users::grant_role(
            &mut tx,
            claim.user_id,
            users::Role::Contractor,
            Some(admin_id),
        )
        .await?;
    }

    claims::record_check(
        &mut tx,
        claim.contractor_id,
        Some(claim_id),
        "manual_review",
        if approve { "pass" } else { "fail" },
        &serde_json::json!({ "note": note, "decided_by": admin_id }),
        Some(admin_id),
    )
    .await?;

    // The single authority. Never a value from the request.
    let outcome = crate::verification::recompute(&mut tx, claim.contractor_id, None).await?;

    audit::record(
        &mut tx,
        AuditEvent::new(
            if approve {
                "claim.approved"
            } else {
                "claim.rejected"
            },
            "contractor_claims",
        )
        .actor(ActorKind::Admin, Some(admin_id))
        .subject(claim_id)
        .data(serde_json::json!({
            "contractor_id": claim.contractor_id,
            "claimant": claim.user_id,
            "verified": outcome.verified,
            "verification_reason": outcome.reason,
        }))
        .request_id(request_id),
    )
    .await?;

    tx.commit().await.map_err(AppError::internal)?;

    // Re-read rather than returning `claim`, which was loaded before the
    // decision was written and therefore still says `pending` with no
    // `decided_at`. Returning it made every response to an approval — and to a
    // rejection — report the claim as still awaiting a decision, so a client
    // rendering `claim.status` would show the moderator that nothing happened.
    //
    // After the commit, so what is returned is what is durably stored. If the
    // re-read fails, the decision still stands and the caller is told the truth
    // about it rather than being handed a stale row.
    let mut conn = pool.acquire().await.map_err(AppError::internal)?;
    let decided = claims::find(&mut conn, claim_id)
        .await?
        .ok_or(AppError::NotFound)?;

    Ok(Decision {
        claim: decided,
        verified: outcome.verified,
        verification_reason: outcome.reason,
    })
}
