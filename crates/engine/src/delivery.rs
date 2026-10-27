//! Notification execution from the same durable outbox as media effects.
//! Destinations and rendered text are prepared once by trusted channel routing.
use crate::{CallError, CallFuture, Outcome, Upstream};
use pitcrew_domain::{
    Effect, Intent, IntentKey,
    channels::{Channel, OutgoingMessage},
    delivery::{DeliveryKey, DeliveryOutcome, DeliveryPurpose},
};
use pitcrew_notify::{AdminAlert, Notifier};
use pitcrew_store::{
    OutboxStatus, Store,
    delivery::{NotificationPayload, RecipientDispatch, ViewerNotification},
};
use std::{collections::BTreeMap, sync::Arc};

pub struct Notifications {
    pub store: Store,
    pub channels: BTreeMap<String, Arc<dyn Channel>>,
    pub notifiers: BTreeMap<String, Arc<dyn Notifier>>,
    /// Same trusted epoch clock used by the worker; injectable in tests.
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Upstream for Notifications {
    fn read<'a>(&'a self, _intent: &'a Intent) -> CallFuture<'a> {
        Box::pin(async { Err(CallError::Unavailable) })
    }

    fn write<'a>(&'a self, intent: &'a Intent) -> CallFuture<'a> {
        Box::pin(async move {
            let purpose = match intent.effect {
                Effect::NotifyAdmin { .. } => DeliveryPurpose::AdminFailure,
                Effect::NotifyViewerFailure { .. } => DeliveryPurpose::ViewerFailure,
                Effect::NotifyReady { .. } => DeliveryPurpose::Ready,
                _ => return Err(CallError::Unavailable),
            };
            // Refuse ad-hoc calls without the worker's durable dispatch claim.
            let rows = self
                .store
                .outbox(intent.job)
                .await
                .map_err(|_| CallError::Unavailable)?;
            if !rows.iter().any(|row| {
                row.intent == *intent && row.status == OutboxStatus::Started && row.fence.is_some()
            }) {
                return Err(CallError::Unavailable);
            }
            let payload = self
                .store
                .notification_payload(IntentKey {
                    job: intent.job,
                    sequence: intent.sequence,
                })
                .await
                .map_err(|_| CallError::Unavailable)?;
            let delivered = match payload {
                NotificationPayload::Viewers { recipients } => {
                    return self.write_fanout(intent, purpose, recipients).await;
                }
                NotificationPayload::Admin {
                    notifier,
                    title,
                    body,
                } => {
                    let adapter = self
                        .notifiers
                        .get(&notifier)
                        .ok_or(CallError::Unavailable)?;
                    adapter
                        .send(&AdminAlert {
                            key: DeliveryKey {
                                job: intent.job,
                                purpose,
                                recipient: notifier,
                            },
                            title,
                            body,
                        })
                        .await
                        .map_err(|_| CallError::Unavailable)?
                }
                NotificationPayload::Viewer {
                    recipient,
                    conversation,
                    text,
                    last_inbound,
                } => {
                    let adapter = self
                        .channels
                        .get(&recipient.channel_id)
                        .ok_or(CallError::Unavailable)?;
                    let capabilities = adapter.capabilities();
                    let now = (self.clock)();
                    if !capabilities.plain_text
                        || text.chars().count() > capabilities.max_text_chars as usize
                        || !capabilities.proactive.permits(last_inbound, now)
                    {
                        return Err(CallError::Unavailable);
                    }
                    let identity =
                        serde_json::to_string(&recipient).map_err(|_| CallError::Unavailable)?;
                    adapter
                        .follow_up(
                            &OutgoingMessage {
                                key: DeliveryKey {
                                    job: intent.job,
                                    purpose,
                                    recipient: identity,
                                },
                                recipient,
                                conversation_id: conversation,
                                text,
                            },
                            last_inbound,
                            now,
                        )
                        .await
                        .map_err(|_| CallError::Unavailable)?
                }
            };
            Ok(if delivered == DeliveryOutcome::Confirmed {
                Outcome::Delivered
            } else {
                Outcome::Uncertain
            })
        })
    }
}

impl Notifications {
    async fn write_fanout(
        &self,
        intent: &Intent,
        purpose: DeliveryPurpose,
        recipients: Vec<ViewerNotification>,
    ) -> Result<Outcome, CallError> {
        let key = IntentKey {
            job: intent.job,
            sequence: intent.sequence,
        };
        let mut complete = true;
        for (index, viewer) in recipients.into_iter().enumerate() {
            let attempt = match self
                .store
                .claim_notification_recipient(key, index as u64)
                .await
                .map_err(|_| CallError::Unavailable)?
            {
                RecipientDispatch::Confirmed => continue,
                RecipientDispatch::Uncertain => {
                    complete = false;
                    continue;
                }
                RecipientDispatch::Send { attempt } => attempt,
            };
            if self.send_fanout_viewer(key, purpose, viewer).await == Ok(DeliveryOutcome::Confirmed)
            {
                self.store
                    .confirm_notification_recipient(key, index as u64, attempt)
                    .await
                    .map_err(|_| CallError::Unavailable)?;
            } else {
                complete = false;
            }
        }
        Ok(if complete {
            Outcome::Delivered
        } else {
            Outcome::Uncertain
        })
    }

    async fn send_fanout_viewer(
        &self,
        key: IntentKey,
        purpose: DeliveryPurpose,
        viewer: ViewerNotification,
    ) -> Result<DeliveryOutcome, CallError> {
        let adapter = self
            .channels
            .get(&viewer.recipient.channel_id)
            .ok_or(CallError::Unavailable)?;
        let capabilities = adapter.capabilities();
        let now = (self.clock)();
        if !capabilities.plain_text
            || viewer.text.chars().count() > capabilities.max_text_chars as usize
            || !capabilities.proactive.permits(viewer.last_inbound, now)
        {
            return Err(CallError::Unavailable);
        }
        let identity =
            serde_json::to_string(&viewer.recipient).map_err(|_| CallError::Unavailable)?;
        adapter
            .follow_up(
                &OutgoingMessage {
                    key: DeliveryKey {
                        job: key.job,
                        purpose,
                        recipient: identity,
                    },
                    recipient: viewer.recipient,
                    conversation_id: viewer.conversation,
                    text: viewer.text,
                },
                viewer.last_inbound,
                now,
            )
            .await
            .map_err(|_| CallError::Unavailable)
    }
}
