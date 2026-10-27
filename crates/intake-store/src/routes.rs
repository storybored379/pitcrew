//! Authenticated bot reply claims bind an intake to an exact platform identity.
//! A Plex account or a reverse lookup of its links alone is never a destination.
use crate::{Result, Store, integer};
use pitcrew_domain::channels::ChannelIdentity;
use sqlx::Row;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedRoute {
    pub recipient: ChannelIdentity,
    pub conversation: String,
    pub last_inbound: u64,
}

impl Store {
    /// The bot reserves its intake reply with the authenticated platform user
    /// in `web_intake_deliveries.account`, and the exact opaque transport key
    /// from `web_intakes.message`. A matching current link must still bind that
    /// user to this report's account. Multiple matching senders fail closed.
    /// This proves routing only; the caller independently refreshes access.
    pub async fn report_route(
        &self,
        report: i64,
        account: &str,
        now: u64,
    ) -> Result<Option<AuthenticatedRoute>> {
        let rows = sqlx::query("SELECT i.channel,i.conversation,i.created,d.account AS sender FROM web_intakes i JOIN web_intake_deliveries d ON d.channel=i.channel AND d.message=i.message JOIN web_links l ON l.channel=d.channel AND l.user=d.account AND l.account=i.account WHERE i.id=? AND i.account=? AND i.created<=? AND json_valid(i.response) AND json_extract(i.response,'$.outcome')='report' LIMIT 2")
            .bind(report).bind(account).bind(integer(now)?).fetch_all(&self.pool).await?;
        let [row] = rows.as_slice() else {
            return Ok(None);
        };
        let channel: String = row.try_get("channel")?;
        let sender: String = row.try_get("sender")?;
        let conversation: String = row.try_get("conversation")?;
        if channel.trim().is_empty() || sender.trim().is_empty() || conversation.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(AuthenticatedRoute {
            recipient: ChannelIdentity {
                channel_id: channel,
                platform_user_id: sender,
            },
            conversation,
            last_inbound: row
                .try_get::<i64, _>("created")?
                .try_into()
                .map_err(|_| crate::Error::Invalid)?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routing_requires_exact_reply_claim_current_account_link_and_unambiguous_sender() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("intake.sqlite"))
            .await
            .unwrap();
        store
            .persist_intake(
                "chat",
                "opaque-message",
                "viewer-a",
                "thread-a",
                &serde_json::json!({"outcome":"report"}),
                10,
            )
            .await
            .unwrap();
        let report = store
            .stored_intake("chat", "opaque-message", "viewer-a")
            .await
            .unwrap()
            .unwrap()
            .id;
        assert_eq!(
            store.report_route(report, "viewer-a", 10).await.unwrap(),
            None
        );
        sqlx::query("INSERT INTO web_links VALUES ('chat','sender-a','viewer-a')")
            .execute(&store.pool)
            .await
            .unwrap();
        // A link alone does not identify this report's sender.
        assert_eq!(
            store.report_route(report, "viewer-a", 10).await.unwrap(),
            None
        );
        store
            .claim_delivery("other-channel", "opaque-message", "sender-a")
            .await
            .unwrap();
        store
            .claim_delivery("chat", "other-message", "sender-a")
            .await
            .unwrap();
        assert_eq!(
            store.report_route(report, "viewer-a", 10).await.unwrap(),
            None
        );
        store
            .claim_delivery("chat", "opaque-message", "sender-a")
            .await
            .unwrap();
        assert_eq!(
            store.report_route(report, "viewer-a", 9).await.unwrap(),
            None
        );
        assert_eq!(
            store.report_route(report, "viewer-b", 10).await.unwrap(),
            None
        );
        let expected = AuthenticatedRoute {
            recipient: ChannelIdentity {
                channel_id: "chat".into(),
                platform_user_id: "sender-a".into(),
            },
            conversation: "thread-a".into(),
            last_inbound: 10,
        };
        assert_eq!(
            store.report_route(report, "viewer-a", 10).await.unwrap(),
            Some(expected.clone())
        );
        sqlx::query("UPDATE web_links SET account='viewer-b'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store.report_route(report, "viewer-a", 10).await.unwrap(),
            None
        );
        sqlx::query("UPDATE web_links SET account='viewer-a'")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO web_links VALUES ('chat','sender-b','viewer-a')")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store.report_route(report, "viewer-a", 10).await.unwrap(),
            Some(expected)
        );
        store
            .claim_delivery("chat", "opaque-message", "sender-b")
            .await
            .unwrap();
        assert_eq!(
            store.report_route(report, "viewer-a", 10).await.unwrap(),
            None
        );
    }
}
