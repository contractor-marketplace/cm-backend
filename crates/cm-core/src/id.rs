//! Identifier generation.
//!
//! Every primary key in the schema is a UUIDv7 generated here, never by the
//! database: the migrations deliberately declare `uuid PRIMARY KEY` with no
//! `DEFAULT`, so a row that reaches Postgres without an application-generated
//! id fails loudly instead of silently acquiring a random v4 and losing the
//! time ordering the index layout depends on.

use uuid::Uuid;

/// A new time-ordered identifier.
pub fn new_id() -> Uuid {
    Uuid::now_v7()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_version_7() {
        let id = new_id();
        assert_eq!(id.get_version_num(), 7, "expected UUIDv7, got {id}");
    }

    #[test]
    fn ids_sort_in_creation_order() {
        let first = new_id();
        // v7 encodes milliseconds; without a gap two ids inside the same
        // millisecond are ordered by the random tail, which proves nothing.
        std::thread::sleep(std::time::Duration::from_millis(3));
        let second = new_id();

        assert!(first < second, "{first} should sort before {second}");
    }

    #[test]
    fn ids_do_not_collide() {
        let ids: HashSet<Uuid> = (0..10_000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 10_000);
    }
}
