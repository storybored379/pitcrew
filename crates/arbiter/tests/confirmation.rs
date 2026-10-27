use pitcrew_arbiter::*;
use pitcrew_domain::{Access, FileIdentity};

fn request() -> MatchedIdRequest {
    MatchedIdRequest {
        job: 11,
        library: 12,
        access: Access::Granted {
            library: 12,
            expires_at: 31,
        },
        now: 30,
        download: 13,
        owned_download: 13,
        import_blocked: true,
        warnings: vec![ImportWarning::MatchedById],
        queue_series: 14,
        proposed_series: 14,
        proposed_episodes: vec![Episode {
            season: 1,
            number: 2,
        }],
        rejection_count: 0,
        filename: "Series.S01E02.mkv".into(),
        bytes: 73_400_320,
        source: FileIdentity {
            id: 15,
            revision: 2,
        },
        decoded_source: Some(FileIdentity {
            id: 15,
            revision: 2,
        }),
    }
}

#[test]
fn only_an_exact_verified_owned_proposal_reaches_the_arbiter() {
    let value = request();
    assert_eq!(
        matched_by_id(&value),
        Ok(VerifiedImportProposal {
            job: 11,
            library: 12,
            download: 13,
            series: 14,
            episodes: [Episode {
                season: 1,
                number: 2
            }]
            .into_iter()
            .collect(),
            source: value.source,
        })
    );
    let mut value = request();
    value.bytes += 1;
    assert!(matched_by_id(&value).is_ok());
}

#[test]
fn every_identity_warning_access_rejection_and_decode_rail_fails_closed() {
    let cases: Vec<(MatchedIdRequest, ImportDenial)> = vec![
        (
            MatchedIdRequest {
                access: Access::Unknown,
                ..request()
            },
            ImportDenial::Access,
        ),
        (
            MatchedIdRequest {
                access: Access::Denied,
                ..request()
            },
            ImportDenial::Access,
        ),
        (
            MatchedIdRequest {
                access: Access::Granted {
                    library: 99,
                    expires_at: 31,
                },
                ..request()
            },
            ImportDenial::Access,
        ),
        (
            MatchedIdRequest {
                now: 31,
                ..request()
            },
            ImportDenial::Access,
        ),
        (
            MatchedIdRequest {
                now: 32,
                ..request()
            },
            ImportDenial::Access,
        ),
        (
            MatchedIdRequest {
                download: 99,
                ..request()
            },
            ImportDenial::Ownership,
        ),
        (
            MatchedIdRequest {
                import_blocked: false,
                ..request()
            },
            ImportDenial::Warnings,
        ),
        (
            MatchedIdRequest {
                warnings: vec![],
                ..request()
            },
            ImportDenial::Warnings,
        ),
        (
            MatchedIdRequest {
                warnings: vec![ImportWarning::MatchedById, ImportWarning::Other],
                ..request()
            },
            ImportDenial::Warnings,
        ),
        (
            MatchedIdRequest {
                proposed_series: 0,
                queue_series: 0,
                ..request()
            },
            ImportDenial::Series,
        ),
        (
            MatchedIdRequest {
                proposed_series: 99,
                ..request()
            },
            ImportDenial::Series,
        ),
        (
            MatchedIdRequest {
                rejection_count: 1,
                ..request()
            },
            ImportDenial::Rejections,
        ),
        (
            MatchedIdRequest {
                bytes: MIN_EPISODE_BYTES - 1,
                ..request()
            },
            ImportDenial::Sample,
        ),
        (
            MatchedIdRequest {
                proposed_episodes: vec![],
                ..request()
            },
            ImportDenial::Episodes,
        ),
        (
            MatchedIdRequest {
                proposed_episodes: vec![Episode {
                    season: 2,
                    number: 2,
                }],
                ..request()
            },
            ImportDenial::Episodes,
        ),
        (
            MatchedIdRequest {
                proposed_episodes: vec![
                    Episode {
                        season: 1,
                        number: 2
                    };
                    2
                ],
                ..request()
            },
            ImportDenial::Episodes,
        ),
        (
            MatchedIdRequest {
                decoded_source: None,
                ..request()
            },
            ImportDenial::UnverifiedSource,
        ),
        (
            MatchedIdRequest {
                decoded_source: Some(FileIdentity {
                    id: 15,
                    revision: 1,
                }),
                ..request()
            },
            ImportDenial::UnverifiedSource,
        ),
    ];
    for (value, expected) in cases {
        assert_eq!(matched_by_id(&value), Err(expected));
    }
}

#[test]
fn sample_and_ambiguous_or_path_episode_tokens_are_rejected() {
    for filename in [
        "Series.S01E02.sample.mkv",
        "Series.S01E02.sample_file.mkv",
        "Series.S01E02.NCOP_extra.mkv",
        "Series.S01E02.!sample.mkv",
        "Series.S01E02.NCOP.mkv",
        "Series.S01E02.NCED.mkv",
    ] {
        assert_eq!(
            matched_by_id(&MatchedIdRequest {
                filename: filename.into(),
                ..request()
            }),
            Err(ImportDenial::Sample)
        );
    }
    for filename in [
        "Series.mkv",
        "S01E02/S01E02.mkv",
        "folder\\S01E02.mkv",
        "S01E02.S01E03.mkv",
        "S01E02-E01.mkv",
        "S01E01-E102.mkv",
        "S01E02E03E04.mkv",
        "S01E02-E99999.mkv",
        "S01E02-99999.mkv",
    ] {
        assert_eq!(
            matched_by_id(&MatchedIdRequest {
                filename: filename.into(),
                ..request()
            }),
            Err(ImportDenial::Episodes),
            "{filename}"
        );
    }
}

#[test]
fn bounded_ranges_and_single_episode_parse_exactly() {
    for filename in [
        "Series.S01E02E03.mkv",
        "Series.S01E02-03.mkv",
        "Series.S01E02-E03.mkv",
    ] {
        let value = MatchedIdRequest {
            filename: filename.into(),
            proposed_episodes: vec![
                Episode {
                    season: 1,
                    number: 2,
                },
                Episode {
                    season: 1,
                    number: 3,
                },
            ],
            ..request()
        };
        assert!(matched_by_id(&value).is_ok(), "{filename}");
    }
    let value = MatchedIdRequest {
        filename: "S01E01-E101.mkv".into(),
        proposed_episodes: (1..=101)
            .map(|number| Episode { season: 1, number })
            .collect(),
        ..request()
    };
    assert!(matched_by_id(&value).is_ok());
    let value = MatchedIdRequest {
        filename: "S01E02-E02.mkv".into(),
        ..request()
    };
    assert!(matched_by_id(&value).is_ok());
}

#[test]
fn another_range_after_a_valid_range_is_ambiguous() {
    let value = MatchedIdRequest {
        filename: "Series.S01E02-03-04.mkv".into(),
        proposed_episodes: vec![
            Episode {
                season: 1,
                number: 2,
            },
            Episode {
                season: 1,
                number: 3,
            },
        ],
        ..request()
    };
    assert_eq!(matched_by_id(&value), Err(ImportDenial::Episodes));
    let value = MatchedIdRequest {
        filename: "Series.S01E02-extra.mkv".into(),
        ..request()
    };
    assert!(matched_by_id(&value).is_ok());
}

#[test]
fn minimum_episode_size_is_seventy_mebibytes_independent_of_the_constant() {
    assert_eq!(
        matched_by_id(&MatchedIdRequest {
            bytes: 73_400_319,
            ..request()
        }),
        Err(ImportDenial::Sample)
    );
    assert!(
        matched_by_id(&MatchedIdRequest {
            bytes: 73_400_320,
            ..request()
        })
        .is_ok()
    );
}
