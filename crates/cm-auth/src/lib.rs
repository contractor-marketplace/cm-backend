//! Authentication: password hashing, sessions, cookies, CSRF and rate limits.
//!
//! Security-sensitive code lives here rather than being spread through
//! handlers, so the rules have one place to be read and one place to be
//! reviewed. Handlers call `service`; nothing outside this crate constructs a
//! session or compares a secret.

pub mod cookie;
pub mod csrf;
pub mod firebase;
pub mod hash;
pub mod login_code;
pub mod mail;
pub mod password;
pub mod ratelimit;
pub mod service;
pub mod token;

pub use service::{
    AuthService, Authenticated, Challenge, IssuedSession, LoginOutcome, LoginResult, RequestContext,
};
