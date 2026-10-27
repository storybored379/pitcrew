//! Composition of the existing bounded read clients. Missing integrations remain
//! unknown. No request is issued at construction, and all transports are injected.
use super::{Clock, CollectedEvidence, EvidenceFuture, EvidenceSource, ResolvedTarget};
use pitcrew_diagnosis::collectors::{
    AgentCollector, AgentQuery, Observation, SessionQuery, TautulliClient, TracearrBridgeClient,
};
use pitcrew_upstream::Transport;

pub struct Collectors<T> {
    pub agent: Option<AgentCollector<T>>,
    pub tautulli: Option<TautulliClient<T>>,
    pub tracearr: Option<TracearrBridgeClient<T>>,
}
impl<T: Transport> EvidenceSource for Collectors<T> {
    fn collect<'a>(
        &'a self,
        target: &'a ResolvedTarget,
        account: &'a str,
        clock: &'a Clock,
    ) -> EvidenceFuture<'a> {
        Box::pin(async move {
            let mut collected = CollectedEvidence::default();
            if let Some(agent) = &self.agent {
                collected.agent = agent
                    .collect_with_clock(
                        AgentQuery {
                            path: &target.agent_path,
                            expected_file: target.agent_file,
                            minutes: 5,
                        },
                        || Some(clock()),
                    )
                    .await;
            }
            let Ok(account_id) = account.parse::<u64>() else {
                return collected;
            };
            if let Some(tautulli) = &self.tautulli {
                collected.sessions = tautulli
                    .history(account_id, &target.plex_rating_key, clock() / 1000, 24)
                    .await;
            }
            if let Some(tracearr) = &self.tracearr {
                let streams = match &self.tautulli {
                    Some(tautulli) => tautulli.stream_count().await,
                    None => Observation::default(),
                };
                let sessions = tracearr
                    .collect(
                        SessionQuery {
                            plex_account_id: account_id,
                            username: None,
                            rating_key: &target.plex_rating_key,
                            hours: 24,
                        },
                        streams,
                    )
                    .await;
                if matches!(sessions, Observation::Available(_)) {
                    collected.sessions = sessions;
                }
            }
            collected
        })
    }
}
