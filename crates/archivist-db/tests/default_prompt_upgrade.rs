//! DB-required integration test: migration 0045 upgrades the untouched
//! 0008-seeded `default` metadata + ocr prompts to the v1.13.0 A/B-validated
//! text on `migrate()`, so a prompt improvement shipped in a release actually
//! reaches the running system (and `get_active_prompt` serves it).
//!
//! Run locally with `DATABASE_URL=postgres://... cargo test -p archivist-db -- --ignored`.

use archivist_core::Stage;
use archivist_db::{connect, get_active_prompt, migrate};

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing to a disposable PostgreSQL 18 database"]
async fn migration_upgrades_seeded_default_prompts_to_v1_13_0() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = connect(&url, 5).await.expect("connect test database");
    migrate(&pool).await.expect("apply migrations");

    let meta = get_active_prompt(&pool, Stage::Metadata)
        .await
        .expect("query active metadata prompt")
        .expect("an active metadata prompt exists");
    assert!(
        meta.content.contains("null is how you omit a key"),
        "active metadata prompt should be the v1.13.0 text after migrate()"
    );
    assert!(
        !meta
            .content
            .contains("It is always better to omit a key, return null, or return []"),
        "the old 0008 metadata default must no longer be active"
    );

    let ocr = get_active_prompt(&pool, Stage::Ocr)
        .await
        .expect("query active ocr prompt")
        .expect("an active ocr prompt exists");
    assert!(
        ocr.content.contains("[blank page]"),
        "active ocr prompt should be the v1.13.0 text after migrate()"
    );
}
