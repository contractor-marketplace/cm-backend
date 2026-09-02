//! `GET /v1/me`.

use crate::extract::CurrentUser;
use axum::Json;
use chrono::{DateTime, Utc};
use cm_db::repo::users::{AccountType, Role, UserStatus};
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct MeResponse {
    user: UserView,
    roles: Vec<Role>,
    session: SessionView,
}

#[derive(Debug, Serialize)]
pub struct UserView {
    id: Uuid,
    /// Null for a federated account whose provider shared no address; the
    /// account page offers to add one.
    email: Option<String>,
    display_name: String,
    status: UserStatus,
    /// Which side of the marketplace. Every client needs this to decide what
    /// to render, and it never changes for the life of the account.
    account_type: AccountType,
    email_verified: bool,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct SessionView {
    id: Uuid,
    expires_at: DateTime<Utc>,
    /// Echoed so a client that lost the cookie — or is not a browser — can
    /// still make a state-changing request. It is derived from the session, so
    /// returning it to the holder of that session discloses nothing new.
    csrf_token: String,
}

pub async fn get_me(CurrentUser(caller): CurrentUser) -> Json<MeResponse> {
    Json(MeResponse {
        user: user_view(&caller.user),
        roles: caller.roles.clone(),
        session: SessionView {
            id: caller.session_id,
            expires_at: caller.session_expires_at,
            csrf_token: caller.csrf_token.clone(),
        },
    })
}

pub(crate) fn user_view(user: &cm_db::repo::users::User) -> UserView {
    UserView {
        id: user.id,
        email: user.email.clone(),
        display_name: user.display_name.clone(),
        status: user.status,
        account_type: user.account_type,
        email_verified: user.email_verified_at.is_some(),
        created_at: user.created_at,
    }
}
