//! Work photos on a claimed listing: added by the owner, capped, shown to
//! everyone on the profile, and out of reach for anyone else.

mod common;

use common::{a_tiny_png, contractor_id, force_claim, router, seed_directory, user_id, Client};
use http::StatusCode;
use sqlx::PgPool;

#[sqlx::test(migrations = "../../migrations")]
async fn the_owner_adds_work_photos_and_everyone_sees_them(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let listing = contractor_id(&pool, "1047382").await;

    let mut owner = Client::new(router.clone());
    owner.register_contractor("owner@example.test").await;
    force_claim(&pool, listing, user_id(&pool, "owner@example.test").await).await;

    let mut anonymous = Client::new(router.clone());
    let profile = format!("/v1/contractors/{listing}");
    assert_eq!(
        anonymous.get(&profile).await.json["photos"]
            .as_array()
            .expect("array")
            .len(),
        0,
        "a listing starts with no work photos, and that is the normal state"
    );

    let path = format!("{profile}/photos");
    let first = owner.post_file(&path, a_tiny_png()).await;
    assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.json);
    assert!(first.json["url"].is_string());
    assert_eq!(first.json["width"], 1);
    owner.post_file(&path, a_tiny_png()).await;

    // Exactly as public as the rest of the profile.
    let photos = anonymous.get(&profile).await.json["photos"].clone();
    let photos = photos.as_array().expect("array");
    assert_eq!(photos.len(), 2);
    assert_ne!(photos[0]["id"], photos[1]["id"], "two distinct photos");

    // Another contractor is a stranger here, and gets a 404 rather than a 403
    // — "that is not yours" would confirm the id is real.
    let mut stranger = Client::new(router.clone());
    stranger.register_contractor("stranger@example.test").await;
    assert_eq!(
        stranger.post_file(&path, a_tiny_png()).await.status,
        StatusCode::NOT_FOUND
    );

    let photo_id = photos[0]["id"].as_str().expect("id").to_owned();
    assert_eq!(
        stranger.delete(&format!("{path}/{photo_id}")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        owner.delete(&format!("{path}/{photo_id}")).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        anonymous.get(&profile).await.json["photos"]
            .as_array()
            .expect("array")
            .len(),
        1
    );
}

#[sqlx::test(migrations = "../../migrations")]
async fn work_photos_are_capped_and_must_be_images(pool: PgPool) {
    seed_directory(&pool).await;
    let router = router(pool.clone());
    let listing = contractor_id(&pool, "1047382").await;

    let mut owner = Client::new(router.clone());
    owner.register_contractor("owner@example.test").await;
    force_claim(&pool, listing, user_id(&pool, "owner@example.test").await).await;
    let path = format!("/v1/contractors/{listing}/photos");

    // Not an image, whatever it claims to be.
    let refused = owner
        .post_file(&path, b"PK\x03\x04 not a photo".to_vec())
        .await;
    assert_eq!(
        refused.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        refused.json
    );

    for _ in 0..12 {
        let added = owner.post_file(&path, a_tiny_png()).await;
        assert_eq!(added.status, StatusCode::CREATED, "{:?}", added.json);
    }

    let thirteenth = owner.post_file(&path, a_tiny_png()).await;
    assert_eq!(thirteenth.status, StatusCode::BAD_REQUEST);
    assert!(
        thirteenth.json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("12"),
        "the message should name the cap: {:?}",
        thirteenth.json
    );
}
