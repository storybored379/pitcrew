use pitcrew_domain::{FileIdentity, channels::*, delivery::*, media::*};

fn identity(
    guid: Option<&str>,
    hash: Option<&str>,
    download: Option<(&str, &str)>,
) -> ReleaseIdentity {
    ReleaseIdentity {
        guid: guid.map(|s| ReleaseGuid {
            source: "indexer-a".into(),
            value: s.to_owned(),
        }),
        infohash: hash.map(|s| InfoHash(s.to_owned())),
        download: download.map(|(client_id, download_id)| DownloadIdentity {
            client_id: client_id.to_owned(),
            download_id: download_id.to_owned(),
        }),
    }
}

#[test]
fn exclusion_matches_any_exact_identifier_without_requiring_same_release_name() {
    let dead = identity(Some("indexer-guid"), Some("A1B2"), Some(("client-a", "42")));
    for matched in [
        identity(
            Some("indexer-guid"),
            Some("different"),
            Some(("other", "43")),
        ),
        identity(Some("other-guid"), Some("a1b2"), Some(("other", "43"))),
        identity(
            Some("other-guid"),
            Some("different"),
            Some(("client-a", "42")),
        ),
    ] {
        assert!(dead.overlaps(&matched));
        assert!(matched.overlaps(&dead));
    }
    for different in [
        ReleaseIdentity::default(),
        identity(Some("other-guid"), None, None),
        identity(None, Some("other-hash"), None),
        identity(None, None, Some(("client-b", "42"))),
        identity(None, None, Some(("client-a", "43"))),
    ] {
        assert!(!dead.overlaps(&different));
        assert!(!different.overlaps(&dead));
    }
}

#[test]
fn empty_identifiers_never_exclude_unknown_releases() {
    for absent in [
        ReleaseIdentity::default(),
        identity(Some(""), None, None),
        ReleaseIdentity {
            guid: Some(ReleaseGuid {
                source: String::new(),
                value: "guid".into(),
            }),
            ..Default::default()
        },
        identity(None, Some(""), None),
        identity(None, None, Some(("", "42"))),
        identity(None, None, Some(("client", ""))),
    ] {
        assert!(!absent.overlaps(&absent));
    }
    let first = identity(Some("same-guid"), None, None);
    let second = ReleaseIdentity {
        guid: Some(ReleaseGuid {
            source: "other-indexer".into(),
            value: "same-guid".into(),
        }),
        ..Default::default()
    };
    assert!(!first.overlaps(&second));
}

#[test]
fn evidence_retains_no_duration_slow_reads_dangling_and_timeout_separately() {
    let evidence = MediaEvidence {
        file: FileIdentity { id: 7, revision: 3 },
        observed_at_unix_ms: 100,
        symlinks: Some(SymlinkEvidence {
            chain: vec![SymlinkHop {
                link: "/media/a".into(),
                target: "/source/a".into(),
            }],
            resolution: SymlinkResolution::Dangling("/source/a".into()),
            elapsed_ms: 5,
            timeout_ms: 1000,
        }),
        mount: Some(MountEvidence {
            mount: "media".into(),
            outcome: ProbeOutcome::TimedOut,
            elapsed_ms: 1000,
            timeout_ms: 1000,
        }),
        reads: vec![TimedRead {
            offset_bytes: 700_000,
            requested_bytes: 1024,
            read_bytes: 0,
            first_byte_ms: None,
            elapsed_ms: 1000,
            timeout_ms: 1000,
            bytes_per_second: 0,
            outcome: ProbeOutcome::TimedOut,
        }],
        header: Some(MediaProbe {
            outcome: ProbeOutcome::Passed,
            duration_ms: None,
            elapsed_ms: 96_000,
            timeout_ms: 180_000,
            faults: vec![MediaFault::NoDuration, MediaFault::SlowRead],
            streams: vec![
                StreamFacts::Video {
                    codec: "hevc".into(),
                    width: 1920,
                    height: 1080,
                    bit_depth: Some(10),
                    dynamic_range: Some("SDR".into()),
                },
                StreamFacts::Audio {
                    codec: "aac".into(),
                    channels: 2,
                    language: Some("eng".into()),
                },
            ],
            tested_ranges: vec![],
        }),
        decode: Some(MediaProbe {
            outcome: ProbeOutcome::Failed,
            duration_ms: None,
            elapsed_ms: 1000,
            timeout_ms: 180_000,
            faults: vec![MediaFault::DecodeError],
            streams: vec![],
            tested_ranges: vec![
                DecodeRange {
                    start_ms: 0,
                    end_ms: Some(30_000),
                },
                DecodeRange {
                    start_ms: 300_000,
                    end_ms: None,
                },
            ],
        }),
        plex_failures: vec![PlexLogFailure {
            at_unix_ms: 90,
            timezone: "Etc/UTC".into(),
            kind: PlexFailureKind::ReadError,
            file: Some(FileIdentity { id: 7, revision: 3 }),
        }],
    };
    let stored = serde_json::to_string(&evidence).unwrap();
    assert_eq!(
        serde_json::from_str::<MediaEvidence>(&stored).unwrap(),
        evidence
    );
}

#[test]
fn source_counters_and_unknown_total_survive_storage_without_inventing_availability() {
    let source = SourceFacts {
        source: SourceKind::NzbdavInfinidysk,
        observed_at_unix_ms: 200,
        release: identity(Some("guid"), None, Some(("client", "download"))),
        facts: vec![
            SourceFact::ProviderMisses {
                provider: "provider-a".into(),
                requests: 100,
                missing: 55,
                window_ms: 60_000,
            },
            SourceFact::DownloadProgress {
                downloaded_bytes: 1000,
                total_bytes: None,
                bytes_per_second: 0,
                complete: false,
            },
            SourceFact::RepairAvailable(false),
            SourceFact::Unknown,
        ],
    };
    let stored = serde_json::to_string(&source).unwrap();
    assert_eq!(
        serde_json::from_str::<SourceFacts>(&stored).unwrap(),
        source
    );
    let legacy = r#"{"outcome":"Passed","duration_ms":null,"elapsed_ms":96000,"timeout_ms":180000,"faults":["NoDuration"]}"#;
    let probe: MediaProbe = serde_json::from_str(legacy).unwrap();
    assert!(probe.streams.is_empty());
    assert!(probe.tested_ranges.is_empty());
    assert_eq!(probe.faults, vec![MediaFault::NoDuration]);
}

#[test]
fn proactive_rules_enforce_first_contact_exact_expiry_and_future_clock() {
    for last in [None, Some(0), Some(100), Some(101)] {
        assert!(ProactiveRule::Allowed.permits(last, 100));
        assert!(!ProactiveRule::Unsupported.permits(last, 100));
    }
    assert!(!ProactiveRule::UserMustMessageFirst.permits(None, 100));
    assert!(ProactiveRule::UserMustMessageFirst.permits(Some(0), 100));
    assert!(ProactiveRule::UserMustMessageFirst.permits(Some(100), 100));
    assert!(!ProactiveRule::UserMustMessageFirst.permits(Some(101), 100));
    let window = ProactiveRule::WithinWindow { seconds: 86_400 };
    assert!(window.permits(Some(10), 10));
    assert!(window.permits(Some(10), 86_409));
    assert!(!window.permits(Some(10), 86_410));
    assert!(!window.permits(Some(10), 86_411));
    assert!(!window.permits(Some(11), 10));
    assert!(!window.permits(None, 10));
    assert!(!ProactiveRule::WithinWindow { seconds: 0 }.permits(Some(10), 10));
}

#[test]
fn generic_link_is_single_use_and_expiry_is_exclusive() {
    let plex = PlexIdentity {
        account_id: "synthetic-account".into(),
    };
    let channel = ChannelIdentity {
        channel_id: "telegram-home".into(),
        platform_user_id: "synthetic-user".into(),
    };
    let original = LinkChallenge {
        code_digest: "stored-digest-only".into(),
        plex: plex.clone(),
        expires_at: 100,
        consumed: false,
    };
    for now in [100, 101] {
        let mut expired = original.clone();
        assert_eq!(
            expired.consume(channel.clone(), now),
            Err(LinkError::Expired)
        );
        assert_eq!(expired, original);
    }
    let mut valid = original;
    assert_eq!(
        valid.consume(channel.clone(), 99).unwrap(),
        LinkedIdentity {
            plex,
            channel: channel.clone(),
            linked_at: 99
        }
    );
    assert!(valid.consumed);
    assert_eq!(
        valid.consume(channel.clone(), 99),
        Err(LinkError::AlreadyUsed)
    );
    assert_eq!(valid.consume(channel, 101), Err(LinkError::AlreadyUsed));
}

fn delivery(purpose: DeliveryPurpose, state: DeliveryState) -> DeliveryRecord {
    DeliveryRecord {
        key: DeliveryKey {
            job: 17,
            purpose,
            recipient: "adapter:recipient".into(),
        },
        state,
        attempt: 1,
    }
}

#[test]
fn confirmed_delivery_is_monotonic_and_each_result_is_honest() {
    for initial in [
        DeliveryState::Pending,
        DeliveryState::Uncertain,
        DeliveryState::Confirmed,
        DeliveryState::PermanentFailure,
    ] {
        for (result, expected) in [
            (DeliveryOutcome::Confirmed, DeliveryState::Confirmed),
            (DeliveryOutcome::NotAccepted, DeliveryState::Pending),
            (DeliveryOutcome::Uncertain, DeliveryState::Uncertain),
            (
                DeliveryOutcome::PermanentFailure,
                DeliveryState::PermanentFailure,
            ),
        ] {
            let mut item = delivery(DeliveryPurpose::Ready, initial);
            let key = item.key.clone();
            let attempt = DeliveryAttempt {
                key: key.clone(),
                sequence: 1,
            };
            item.observe(&attempt, result).unwrap();
            assert_eq!(
                item.state,
                if initial == DeliveryState::Confirmed {
                    DeliveryState::Confirmed
                } else {
                    expected
                }
            );
            assert_eq!(item.key, key);
        }
    }
}

#[test]
fn uncertain_ready_needs_deduplication_before_retrying_same_key() {
    for purpose in [
        DeliveryPurpose::Ready,
        DeliveryPurpose::ViewerFailure,
        DeliveryPurpose::AdminFailure,
    ] {
        for deduplicates in [false, true] {
            for (state, expected) in [
                (DeliveryState::Pending, DeliveryAction::RetrySameKey),
                (DeliveryState::Confirmed, DeliveryAction::Complete),
                (
                    DeliveryState::PermanentFailure,
                    DeliveryAction::ReviewFailure,
                ),
                (
                    DeliveryState::Uncertain,
                    if purpose == DeliveryPurpose::Ready && !deduplicates {
                        DeliveryAction::ReconcileOrReview
                    } else {
                        DeliveryAction::RetrySameKey
                    },
                ),
            ] {
                assert_eq!(delivery(purpose, state).next_action(deduplicates), expected);
            }
        }
    }
}

#[test]
fn delayed_rejection_cannot_reopen_an_uncertain_ready_for_resend() {
    let mut item = delivery(DeliveryPurpose::Ready, DeliveryState::Pending);
    let first = item.begin_attempt(false).unwrap();
    item.observe(&first, DeliveryOutcome::NotAccepted).unwrap();
    let second = item.begin_attempt(false).unwrap();
    assert_eq!(first.key, second.key);
    assert_eq!(second.sequence, first.sequence + 1);
    // A delayed rejection from the first attempt must not reset the second.
    assert_eq!(
        item.observe(&first, DeliveryOutcome::NotAccepted),
        Err(DeliveryError::StaleAttempt)
    );
    assert_eq!(item.next_action(false), DeliveryAction::ReconcileOrReview);
    assert_eq!(item.begin_attempt(false), Err(DeliveryError::NotRetryable));
    item.observe(&second, DeliveryOutcome::Confirmed).unwrap();
    assert_eq!(item.state, DeliveryState::Confirmed);
}

#[test]
fn attempt_correlation_covers_cross_job_and_no_dispatch_yet() {
    let mut item = delivery(DeliveryPurpose::Ready, DeliveryState::Pending);
    let mut wrong = DeliveryAttempt {
        key: item.key.clone(),
        sequence: item.attempt,
    };
    wrong.key.job += 1;
    assert_eq!(
        item.observe(&wrong, DeliveryOutcome::Confirmed),
        Err(DeliveryError::StaleAttempt)
    );
    wrong.key = item.key.clone();
    wrong.sequence += 1;
    assert_eq!(
        item.observe(&wrong, DeliveryOutcome::Confirmed),
        Err(DeliveryError::StaleAttempt)
    );
    item.attempt = 0;
    wrong.sequence = 0;
    assert_eq!(
        item.observe(&wrong, DeliveryOutcome::Confirmed),
        Err(DeliveryError::StaleAttempt)
    );
    assert_eq!(item.state, DeliveryState::Pending);
}

#[test]
fn begin_attempt_records_uncertainty_before_io_and_never_wraps_sequence() {
    for state in [
        DeliveryState::Pending,
        DeliveryState::Uncertain,
        DeliveryState::Confirmed,
        DeliveryState::PermanentFailure,
    ] {
        let mut item = delivery(DeliveryPurpose::Ready, state);
        let before = item.clone();
        if matches!(state, DeliveryState::Pending | DeliveryState::Uncertain) {
            let token = item.begin_attempt(true).unwrap();
            assert_eq!(
                token,
                DeliveryAttempt {
                    key: before.key.clone(),
                    sequence: 2
                }
            );
            assert_eq!(item.attempt, 2);
            assert_eq!(item.state, DeliveryState::Uncertain);
            assert_eq!(item.key, before.key);
        } else {
            assert_eq!(item.begin_attempt(true), Err(DeliveryError::NotRetryable));
            assert_eq!(item, before);
        }
    }
    let mut exhausted = delivery(DeliveryPurpose::Ready, DeliveryState::Pending);
    exhausted.attempt = u64::MAX;
    let before = exhausted.clone();
    assert_eq!(
        exhausted.begin_attempt(true),
        Err(DeliveryError::AttemptOverflow)
    );
    assert_eq!(exhausted, before);
}
