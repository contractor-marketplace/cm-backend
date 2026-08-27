//! Object storage for job photos.
//!
//! Two responsibilities, deliberately together: turning an upload into something
//! safe to publish (`image`), and putting it somewhere (`gcs`). They are one
//! crate because they are one decision — a byte sequence only becomes storable
//! by going through the normaliser, and there is no public way to store bytes
//! that skipped it. `Store::put` takes a `Normalised`, not a `Vec<u8>`.

pub mod gcs;
pub mod image;

pub use crate::image::{normalise, Normalised, MAX_EDGE};

use cm_core::AppError;
use std::sync::{Arc, Mutex};

/// Where photos go.
///
/// An enum rather than a trait object: there are exactly two implementations and
/// there is no plausible third, so the dispatch is not worth a `dyn` and the
/// match is worth reading.
#[derive(Clone)]
pub enum Store {
    /// Production. A bucket, and objects readable by anyone.
    Gcs(gcs::Bucket),
    /// Tests, and only tests. Holds objects in memory so the suite needs no
    /// network and no credentials.
    Memory(MemoryStore),
}

impl Store {
    pub fn memory() -> Self {
        Self::Memory(MemoryStore::default())
    }

    /// Store a normalised image and return the URL it will be served from.
    ///
    /// Takes `Normalised` rather than bytes so there is no way to reach storage
    /// with a file that skipped the metadata-stripping pass.
    pub async fn put(&self, key: &str, image: &Normalised) -> Result<String, AppError> {
        match self {
            Self::Gcs(bucket) => bucket.put(key, &image.bytes).await,
            Self::Memory(store) => Ok(store.put(key, image.bytes.clone())),
        }
    }

    pub async fn delete(&self, key: &str) -> Result<(), AppError> {
        match self {
            Self::Gcs(bucket) => bucket.delete(key).await,
            Self::Memory(store) => {
                store.delete(key);
                Ok(())
            }
        }
    }

    /// The public URL for a key, without a round trip. Used when reading rows
    /// back, where the object is known to exist.
    pub fn url_for(&self, key: &str) -> String {
        match self {
            Self::Gcs(bucket) => bucket.url_for(key),
            Self::Memory(store) => store.url_for(key),
        }
    }

    /// Whether this store actually persists anything. `check-config` refuses to
    /// start a production server where this is false, so the in-memory store can
    /// never be a silent downgrade in production.
    pub fn is_durable(&self) -> bool {
        matches!(self, Self::Gcs(_))
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Gcs(bucket) => format!("gcs://{}", bucket.name()),
            Self::Memory(_) => "in-memory (NOT durable)".to_owned(),
        }
    }
}

/// In-memory objects, for tests.
#[derive(Clone, Default)]
pub struct MemoryStore {
    objects: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

impl MemoryStore {
    fn put(&self, key: &str, bytes: Vec<u8>) -> String {
        self.objects
            .lock()
            .expect("the object map is never held across a panic")
            .insert(key.to_owned(), bytes);
        self.url_for(key)
    }

    fn delete(&self, key: &str) {
        self.objects
            .lock()
            .expect("the object map is never held across a panic")
            .remove(key);
    }

    fn url_for(&self, key: &str) -> String {
        format!("memory:///{key}")
    }

    /// For assertions: what is actually stored under a key.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.objects
            .lock()
            .expect("the object map is never held across a panic")
            .get(key)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.objects
            .lock()
            .expect("the object map is never held across a panic")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The object key for a job photo. One function so the layout is decided once
/// and a delete can never disagree with the put that created it.
pub fn photo_key(job_id: uuid::Uuid, photo_id: uuid::Uuid) -> String {
    format!("jobs/{job_id}/{photo_id}.jpg")
}

/// The object key for a contractor's profile photo.
///
/// Carries a fresh `photo_id` rather than living at a fixed
/// `contractors/{id}.jpg`, so replacing a photo writes a new object instead of
/// overwriting one. A fixed key would be served stale from every cache between
/// here and the viewer for as long as they cache it, and the URL would give no
/// way to tell the versions apart. The displaced object is deleted by the
/// caller once the row points at the new one.
pub fn contractor_photo_key(contractor_id: uuid::Uuid, photo_id: uuid::Uuid) -> String {
    format!("contractors/{contractor_id}/{photo_id}.jpg")
}
