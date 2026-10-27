use pitcrew_intake_store::Store;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

#[tokio::test]
async fn matching_reply_claims_with_blank_route_fields_never_become_destinations() {
    for (channel, sender, conversation) in [
        ("", "sender-a", "thread-a"),
        (" \t", "sender-a", "thread-a"),
        ("chat", "", "thread-a"),
        ("chat", " \t", "thread-a"),
        ("chat", "sender-a", ""),
        ("chat", "sender-a", " \t"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("intake.sqlite");
        let store = Store::open(&path).await.unwrap();
        store
            .persist_intake(
                channel,
                "message-a",
                "account-a",
                conversation,
                &serde_json::json!({"outcome":"report"}),
                10,
            )
            .await
            .unwrap();
        let report = store
            .stored_intake(channel, "message-a", "account-a")
            .await
            .unwrap()
            .unwrap();
        store
            .claim_delivery(channel, "message-a", sender)
            .await
            .unwrap();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().filename(path))
            .await
            .unwrap();
        sqlx::query("INSERT INTO web_links(channel,user,account) VALUES (?,?,'account-a')")
            .bind(channel)
            .bind(sender)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            store
                .report_route(report.id, "account-a", 10)
                .await
                .unwrap(),
            None
        );
    }
}
