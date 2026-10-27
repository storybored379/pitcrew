//! Deterministic upstream fault scripts for tests and scenario adapters.
//!
//! Effects and responses are separate: a lost response may follow an accepted mutation.
use std::collections::VecDeque;

pub mod holding;
pub mod queue_actor;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply<T> {
    Success(T),
    Timeout,
    ServerError(u16),
    AcceptedThenLost(T),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptedCall<T> {
    pub operation: String,
    pub reply: Reply<T>,
}

#[derive(Debug)]
pub struct ScriptedUpstream<T> {
    pending: VecDeque<ScriptedCall<T>>,
    pub calls: Vec<String>,
    /// Mutations accepted upstream, even if their response was lost.
    pub accepted: Vec<T>,
}

impl<T: Clone> ScriptedUpstream<T> {
    pub fn new(script: impl IntoIterator<Item = ScriptedCall<T>>) -> Self {
        Self {
            pending: script.into_iter().collect(),
            calls: Vec::new(),
            accepted: Vec::new(),
        }
    }

    pub fn call(&mut self, operation: &str) -> Result<T, String> {
        let next = self.pending.front().ok_or("unexpected upstream call")?;
        if next.operation != operation {
            return Err(format!("expected {}, got {operation}", next.operation));
        }
        let next = self.pending.pop_front().expect("front was present");
        self.calls.push(operation.to_owned());
        match next.reply {
            Reply::Success(value) => {
                self.accepted.push(value.clone());
                Ok(value)
            }
            Reply::AcceptedThenLost(value) => {
                self.accepted.push(value);
                Err("response lost".into())
            }
            Reply::Timeout => Err("timeout before acceptance".into()),
            Reply::ServerError(status) => Err(format!("upstream HTTP {status}")),
        }
    }

    pub fn is_exhausted(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertain_response_retains_real_upstream_acceptance() {
        let mut upstream = ScriptedUpstream::new([
            ScriptedCall {
                operation: "grab".into(),
                reply: Reply::AcceptedThenLost(7),
            },
            ScriptedCall {
                operation: "history".into(),
                reply: Reply::Success(7),
            },
        ]);
        assert!(upstream.call("delete").is_err());
        assert!(upstream.calls.is_empty());
        assert!(upstream.call("grab").is_err());
        assert_eq!(upstream.accepted, [7]);
        assert_eq!(upstream.call("history"), Ok(7));
        assert!(upstream.is_exhausted());
        assert!(upstream.call("grab").is_err());
    }
}
