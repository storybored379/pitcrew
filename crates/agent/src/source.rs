//! Facts-only nzbdav/InfiniDysk contract. This normalized transport is deliberately
//! not an assertion about any deployed version's HTTP or database schema.

use pitcrew_domain::media::{
    ReleaseIdentity, SourceAdapter, SourceError, SourceFact, SourceFacts, SourceFuture, SourceKind,
    SourceRepair,
};
use std::{path::Path, time::Duration};

/// Local observations supplied by a read-only, separately certified transport.
/// A provider label is an opaque local alias, never a hostname or credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderObservation {
    pub provider: String,
    pub available: Option<bool>,
    pub requests: u64,
    pub missing: u64,
    pub window_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NzbdavSnapshot {
    pub observed_at_unix_ms: u64,
    pub release: ReleaseIdentity,
    pub client_available: bool,
    pub providers: Vec<ProviderObservation>,
    pub progress: Option<DownloadProgress>,
    pub unpack_failed: bool,
    pub download_failed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub bytes_per_second: u64,
    /// A source completion claim does not prove that a media file is playable.
    pub complete: bool,
}

/// No mutation method exists in this transport. Implementations must bound their
/// response body and must be cancellation-safe; timeout drops the future.
pub trait NzbdavFactsTransport: Send + Sync {
    fn snapshot<'a>(&'a self, path: &'a Path) -> SourceFuture<'a, NzbdavSnapshot>;
}

pub struct NzbdavAdapter<T> {
    transport: T,
    timeout: Duration,
}

impl<T: NzbdavFactsTransport> NzbdavAdapter<T> {
    pub fn new(transport: T, timeout: Duration) -> Result<Self, SourceError> {
        if timeout.is_zero() {
            return Err(SourceError::Unsupported);
        }
        Ok(Self { transport, timeout })
    }
}

impl<T: NzbdavFactsTransport> SourceAdapter for NzbdavAdapter<T> {
    fn kind(&self) -> SourceKind {
        SourceKind::NzbdavInfinidysk
    }

    fn facts<'a>(&'a self, path: &'a Path) -> SourceFuture<'a, SourceFacts> {
        Box::pin(async move {
            let snapshot = tokio::time::timeout(self.timeout, self.transport.snapshot(path))
                .await
                .map_err(|_| SourceError::TimedOut)??;
            if snapshot.providers.len() > 64 {
                return Err(SourceError::Unavailable);
            }
            // Unknown explicitly marks the version/transport contract unverified.
            // Repair remains disabled even when an upstream advertises it.
            let mut facts = vec![
                SourceFact::Unknown,
                SourceFact::RepairAvailable(false),
                SourceFact::ClientAvailable(snapshot.client_available),
            ];
            for provider in snapshot.providers {
                if provider.provider.is_empty()
                    || provider.provider.len() > 128
                    || provider.missing > provider.requests
                    || provider.window_ms == 0
                {
                    return Err(SourceError::Unavailable);
                }
                if let Some(available) = provider.available {
                    facts.push(SourceFact::ProviderAvailability {
                        provider: provider.provider.clone(),
                        available,
                    });
                }
                facts.push(SourceFact::ProviderMisses {
                    provider: provider.provider,
                    requests: provider.requests,
                    missing: provider.missing,
                    window_ms: provider.window_ms,
                });
            }
            if let Some(progress) = snapshot.progress {
                facts.push(SourceFact::DownloadProgress {
                    downloaded_bytes: progress.downloaded_bytes,
                    total_bytes: progress.total_bytes,
                    bytes_per_second: progress.bytes_per_second,
                    complete: progress.complete,
                });
            }
            if snapshot.unpack_failed {
                facts.push(SourceFact::UnpackFailed);
            }
            if snapshot.download_failed {
                facts.push(SourceFact::DownloadFailed);
            }
            Ok(SourceFacts {
                source: self.kind(),
                observed_at_unix_ms: snapshot.observed_at_unix_ms,
                release: snapshot.release,
                facts,
            })
        })
    }

    fn repair(&self) -> Option<&dyn SourceRepair> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pitcrew_domain::media::DownloadIdentity;

    struct Fixture(Result<NzbdavSnapshot, SourceError>);
    impl NzbdavFactsTransport for Fixture {
        fn snapshot<'a>(&'a self, path: &'a Path) -> SourceFuture<'a, NzbdavSnapshot> {
            assert_eq!(path, Path::new("/synthetic/movie.mkv"));
            Box::pin(async { self.0.clone() })
        }
    }

    fn snapshot() -> NzbdavSnapshot {
        NzbdavSnapshot {
            observed_at_unix_ms: 47,
            release: ReleaseIdentity {
                download: Some(DownloadIdentity {
                    client_id: "synthetic-client".into(),
                    download_id: "job-7".into(),
                }),
                ..ReleaseIdentity::default()
            },
            client_available: true,
            providers: vec![ProviderObservation {
                provider: "primary".into(),
                available: Some(false),
                requests: 10,
                missing: 9,
                window_ms: 1000,
            }],
            progress: Some(DownloadProgress {
                downloaded_bytes: 120,
                total_bytes: Some(120),
                bytes_per_second: 3,
                complete: true,
            }),
            unpack_failed: true,
            download_failed: true,
        }
    }

    async fn facts(snapshot: NzbdavSnapshot) -> Result<SourceFacts, SourceError> {
        NzbdavAdapter::new(Fixture(Ok(snapshot)), Duration::from_millis(30))
            .unwrap()
            .facts(Path::new("/synthetic/movie.mkv"))
            .await
    }

    #[tokio::test]
    async fn preserves_facts_without_enabling_repair_or_claiming_verification() {
        let input = snapshot();
        let output = facts(input.clone()).await.unwrap();
        assert_eq!(output.source, SourceKind::NzbdavInfinidysk);
        assert_eq!(output.observed_at_unix_ms, 47);
        assert_eq!(output.release, input.release);
        assert_eq!(
            output.facts,
            vec![
                SourceFact::Unknown,
                SourceFact::RepairAvailable(false),
                SourceFact::ClientAvailable(true),
                SourceFact::ProviderAvailability {
                    provider: "primary".into(),
                    available: false
                },
                SourceFact::ProviderMisses {
                    provider: "primary".into(),
                    requests: 10,
                    missing: 9,
                    window_ms: 1000
                },
                SourceFact::DownloadProgress {
                    downloaded_bytes: 120,
                    total_bytes: Some(120),
                    bytes_per_second: 3,
                    complete: true
                },
                SourceFact::UnpackFailed,
                SourceFact::DownloadFailed,
            ]
        );
        let adapter = NzbdavAdapter::new(Fixture(Ok(input)), Duration::from_secs(1)).unwrap();
        assert!(adapter.repair().is_none());
    }

    #[tokio::test]
    async fn absent_observations_and_errors_are_never_invented() {
        let mut input = snapshot();
        input.client_available = false;
        input.providers[0].available = None;
        input.progress = None;
        input.unpack_failed = false;
        input.download_failed = false;
        assert_eq!(
            facts(input).await.unwrap().facts,
            vec![
                SourceFact::Unknown,
                SourceFact::RepairAvailable(false),
                SourceFact::ClientAvailable(false),
                SourceFact::ProviderMisses {
                    provider: "primary".into(),
                    requests: 10,
                    missing: 9,
                    window_ms: 1000
                },
            ]
        );
        for error in [
            SourceError::Unsupported,
            SourceError::Unavailable,
            SourceError::TimedOut,
        ] {
            let adapter = NzbdavAdapter::new(Fixture(Err(error)), Duration::from_secs(1)).unwrap();
            assert_eq!(
                adapter.facts(Path::new("/synthetic/movie.mkv")).await,
                Err(error)
            );
        }
    }

    #[tokio::test]
    async fn rejects_invalid_and_accepts_boundary_provider_observations() {
        for mutate in [
            |s: &mut NzbdavSnapshot| s.providers[0].provider.clear(),
            |s: &mut NzbdavSnapshot| s.providers[0].provider = "a".repeat(129),
            |s: &mut NzbdavSnapshot| s.providers[0].missing = 11,
            |s: &mut NzbdavSnapshot| s.providers[0].window_ms = 0,
            |s: &mut NzbdavSnapshot| s.providers = vec![s.providers[0].clone(); 65],
        ] {
            let mut input = snapshot();
            mutate(&mut input);
            assert_eq!(facts(input).await, Err(SourceError::Unavailable));
        }
        let mut input = snapshot();
        input.providers[0].provider = "p".repeat(128);
        input.providers[0].missing = 10;
        input.providers[0].window_ms = 1;
        input.providers = vec![input.providers[0].clone(); 64];
        assert!(facts(input).await.is_ok());
    }

    struct Pending;
    impl NzbdavFactsTransport for Pending {
        fn snapshot<'a>(&'a self, _: &'a Path) -> SourceFuture<'a, NzbdavSnapshot> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn transport_deadline_and_configuration_fail_closed() {
        assert!(NzbdavAdapter::new(Pending, Duration::ZERO).is_err());
        let adapter = NzbdavAdapter::new(Pending, Duration::from_millis(1)).unwrap();
        assert_eq!(
            adapter.facts(Path::new("/synthetic/movie.mkv")).await,
            Err(SourceError::TimedOut)
        );
    }
}
