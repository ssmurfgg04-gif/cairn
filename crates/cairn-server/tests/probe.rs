use sqlx::Row;
#[tokio::test]
async fn probe_tables() {
    let dir = tempfile::tempdir().unwrap();
    let pool = cairn_server::db::open(&dir.path().join("meta.db")).await.unwrap();
    cairn_server::db::migrate(&pool).await.unwrap();
    let rows = sqlx::query("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .fetch_all(&pool).await.unwrap();
    let names: Vec<String> = rows.iter().map(|r| r.get::<String, _>(0)).collect();
    println!("TABLES: {:?}", names);
    let stmts = sqlx::query("PRAGMA database_list").fetch_all(&pool).await.unwrap();
    for s in &stmts {
        println!("DB: {:?}", s.get::<String, _>(1));
    }
}
