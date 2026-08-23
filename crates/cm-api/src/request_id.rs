//! Request identifiers.
//!
//! Every request gets a UUIDv7, echoed in `x-request-id` and attached to every
//! log line it produces, so "it failed at 14:03" becomes an exact trace.
//! Time-ordered ids also make a search by id range useful.
//!
//! Any inbound `x-request-id` is stripped by the router before this runs: a
//! client-supplied value could collide with, or forge, an id in our own logs.

use cm_core::new_id;
use http::Request;
use tower_http::request_id::{MakeRequestId, RequestId};

#[derive(Debug, Clone, Copy, Default)]
pub struct MakeUuidV7RequestId;

impl MakeRequestId for MakeUuidV7RequestId {
    fn make_request_id<B>(&mut self, _request: &Request<B>) -> Option<RequestId> {
        new_id().to_string().parse().ok().map(RequestId::new)
    }
}
