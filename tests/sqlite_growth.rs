//! SQLite 体积控制（§22）。

#[tokio::test]
async fn new_databases_use_incremental_auto_vacuum_and_a_bounded_wal() {
    let dir = tempfile::tempdir().unwrap();
    let pool = akhub::storage::open(dir.path()).await.unwrap();
    let mode: i64 = sqlx::query_scalar("PRAGMA auto_vacuum")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mode, 2, "新库应为 INCREMENTAL");
    let limit: i64 = sqlx::query_scalar("PRAGMA journal_size_limit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(limit, 64 * 1024 * 1024);

    let store = akhub::storage::Store::new(pool);
    assert!(store.incremental_vacuum(100).await.unwrap());
}
