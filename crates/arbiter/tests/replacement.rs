use pitcrew_arbiter::{ImportDenial, MIN_EPISODE_BYTES, ReplacementProof, replacement};
use pitcrew_domain::FileIdentity;

fn proof() -> ReplacementProof {
    ReplacementProof {
        original: FileIdentity { id: 1, revision: 2 },
        current: FileIdentity { id: 1, revision: 2 },
        source: FileIdentity { id: 3, revision: 4 },
        decoded_source: Some(FileIdentity { id: 3, revision: 4 }),
        owned_download: 5,
        download: 5,
        exact_target: true,
        candidate_count: 1,
        rejection_count: Some(0),
        filename: "Synthetic.S01E02.mkv".into(),
        bytes: Some(MIN_EPISODE_BYTES),
    }
}

#[test]
fn healthy_samples_and_unknown_sizes_do_not_prove_complete_replacements() {
    for bytes in [None, Some(0), Some(MIN_EPISODE_BYTES - 1)] {
        let mut p = proof();
        p.bytes = bytes;
        assert_eq!(replacement(&p), Err(ImportDenial::Sample));
    }
    for filename in [
        "sample.mkv",
        "Synthetic.sample.mkv",
        "Synthetic.!sample.mkv",
        "Synthetic.NCOP.mkv",
        "Synthetic.NCED.mkv",
        "Synthetic_SaMpLe_extra.mkv",
    ] {
        let mut p = proof();
        p.filename = filename.into();
        assert_eq!(replacement(&p), Err(ImportDenial::Sample), "{filename}");
    }
    for filename in [
        "Synthetic.S01E02.mkv",
        "Samples.S01E02.mkv",
        "Examples.S01E02.mkv",
    ] {
        let mut p = proof();
        p.filename = filename.into();
        assert_eq!(replacement(&p), Ok(()));
    }
}

#[test]
fn every_replacement_proof_is_required() {
    assert_eq!(replacement(&proof()), Ok(()));
    let mut p = proof();
    p.current.revision += 1;
    assert_eq!(replacement(&p), Err(ImportDenial::Ownership));
    let mut p = proof();
    p.owned_download += 1;
    assert_eq!(replacement(&p), Err(ImportDenial::Ownership));
    let mut p = proof();
    p.exact_target = false;
    assert_eq!(replacement(&p), Err(ImportDenial::Episodes));
    for count in [0, 2] {
        let mut p = proof();
        p.candidate_count = count;
        assert_eq!(replacement(&p), Err(ImportDenial::Episodes));
    }
    for count in [None, Some(1)] {
        let mut p = proof();
        p.rejection_count = count;
        assert_eq!(replacement(&p), Err(ImportDenial::Rejections));
    }
    for file in [None, Some(FileIdentity { id: 3, revision: 5 })] {
        let mut p = proof();
        p.decoded_source = file;
        assert_eq!(replacement(&p), Err(ImportDenial::UnverifiedSource));
    }
}
