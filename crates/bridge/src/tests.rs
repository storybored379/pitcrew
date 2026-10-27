use super::{Authority, Diagnosis, valid};
use pitcrew_diagnosis::{
    facts::{DecodeOutcome, EvidenceFacts, ProbeOutcome},
    rules::classify,
};
use pitcrew_domain::{Access, FileIdentity};

fn provenance() -> (Diagnosis, Authority) {
    let mut facts = EvidenceFacts::default();
    facts.playback.decode = DecodeOutcome::Bad;
    let diagnosis = Diagnosis {
        report_id: 1,
        decided_at: 10,
        decision: classify(&facts),
        evidence_summary: facts,
        resource: "library-a:episode-a".into(),
        library_id: 1,
        arr_instance_id: "series-a".into(),
        arr_item_id: 1,
        arr_file_id: 1,
        agent_file: FileIdentity { id: 1, revision: 1 },
    };
    let authority = Authority {
        account: "viewer-a".into(),
        resource: diagnosis.resource.clone(),
        library: 1,
        arr_instance_id: diagnosis.arr_instance_id.clone(),
        arr_item_id: diagnosis.arr_item_id,
        arr_file_id: diagnosis.arr_file_id,
        file: diagnosis.agent_file,
        observed_at: 10,
        access: Access::Granted {
            library: 1,
            expires_at: 1000,
        },
        probe: ProbeOutcome::Unknown,
        decode: DecodeOutcome::Bad,
        route: None,
    };
    (diagnosis, authority)
}

#[test]
fn local_provenance_rejects_unbound_or_incomplete_identity_before_registry_lookup() {
    let (diagnosis, authority) = provenance();
    assert!(valid(&diagnosis, &authority, "viewer-a", 10, 300));
    // A registry must never mask a weakened provenance contract. Keep each
    // tuple mutually consistent so rejection is specifically for a missing
    // identity component, rather than a mismatch between trusted inputs.
    for component in [
        "agent id",
        "agent revision",
        "arr item",
        "arr file",
        "resource",
        "arr instance",
        "account binding",
    ] {
        let mut d = diagnosis.clone();
        let mut a = authority.clone();
        match component {
            "agent id" => {
                d.agent_file.id = 0;
                a.file.id = 0;
            }
            "agent revision" => {
                d.agent_file.revision = 0;
                a.file.revision = 0;
            }
            "arr item" => {
                d.arr_item_id = 0;
                a.arr_item_id = 0;
            }
            "arr file" => {
                d.arr_file_id = 0;
                a.arr_file_id = 0;
            }
            "resource" => {
                d.resource.clear();
                a.resource.clear();
            }
            "arr instance" => {
                d.arr_instance_id.clear();
                a.arr_instance_id.clear();
            }
            "account binding" => a.account = "another-viewer".into(),
            _ => unreachable!(),
        }
        assert!(
            !valid(&d, &a, "viewer-a", 10, 300),
            "invalid {component} must not be accepted as local authority"
        );
    }
}

#[test]
fn local_provenance_observation_decision_and_grant_boundaries() {
    for (observed, decided, now, expires, expected) in [
        (10, 10, 10, 1000, true),
        (10, 10, 310, 1000, true),
        (10, 10, 311, 1000, false),
        (11, 10, 10, 1000, false),
        (11, 11, 10, 1000, false),
        (10, 9, 10, 1000, false),
        (10, 10, 10, 10, false),
        (10, 10, 10, 11, true),
    ] {
        let (mut d, mut a) = provenance();
        d.decided_at = decided;
        a.observed_at = observed;
        a.access = Access::Granted {
            library: 1,
            expires_at: expires,
        };
        assert_eq!(
            valid(&d, &a, "viewer-a", now, 300),
            expected,
            "observed={observed}, decided={decided}, now={now}, grant expires={expires}"
        );
    }
}
