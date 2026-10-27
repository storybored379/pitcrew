//! Privacy is an allow-list projection, not a best-effort regular expression.
use pitcrew_diagnosis::facts::EvidenceFacts;
use serde::Serialize;
use serde_json::Value;

/// Private local evidence may contain arbitrary upstream metadata and viewer
/// text. Deliberately neither Debug nor Serialize; never pass this to transport.
pub struct LocalEvidence {
    pub facts: EvidenceFacts,
    pub viewer_text: String,
    pub private_context: Value,
}

/// Only the redactor can construct this boundary type. No Deserialize or mutable
/// access: unknown strings, identifiers, paths and viewer instructions cannot be
/// smuggled into provider input. Facts contain closed enums and measurements only.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RedactedEvidence {
    facts: EvidenceFacts,
}
impl RedactedEvidence {
    pub fn facts(&self) -> &EvidenceFacts {
        &self.facts
    }
}

/// Drop ALL free text, including benign viewer words. This deliberately trades
/// free-text model interpretation for a verifiable privacy boundary. Local intake
/// extracts typed complaint/evidence facts before this point.
pub fn redact(local: &LocalEvidence) -> RedactedEvidence {
    RedactedEvidence { facts: local.facts }
}
