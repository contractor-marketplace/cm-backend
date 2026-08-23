//! The HTTP surface.
//!
//! Handlers stay thin: they read state, call a service, and shape a response.
//! A handler that contains a business rule is a review failure — the rule
//! belongs where it can be tested without a request.

pub mod client_ip;
pub mod extract;
pub mod handlers;
pub mod health;
pub mod middleware;
pub mod request_id;
pub mod router;
pub mod state;

pub use router::build;
pub use state::{AppState, BuildInfo};
