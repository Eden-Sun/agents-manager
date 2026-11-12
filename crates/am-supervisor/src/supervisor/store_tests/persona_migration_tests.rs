use super::*;

#[tokio::test]
async fn first_upgrade_preserves_custom_bot_persona_and_embedded_baseline() {
    let p = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
        .connect("sqlite::memory:").await.unwrap();
    migrate(&p).await.unwrap();
    get_or_init(&p).await.unwrap();
    assert!(seed_persona_from(&p, "custom newer text", "legacy_bot", "old binary text").await.unwrap());
    let row = get_or_init(&p).await.unwrap();
    assert_eq!(row.persona_text.as_deref(), Some("custom newer text"));
    assert_eq!(row.persona_source.as_deref(), Some("legacy_bot"));
    assert_eq!(row.persona_seed_hash, Some(super::super::persona::hash("old binary text")));
    assert!(!seed_persona_if_empty(&p, "another binary").await.unwrap());
    assert!(!seed_persona_from(&p, "outdated bot projection", "legacy_bot", "old binary").await.unwrap());
    assert_eq!(get_or_init(&p).await.unwrap().persona_text.as_deref(), Some("custom newer text"));
}
