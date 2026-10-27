//! Offline import of a standalone legacy SQLite snapshot. This module neither
//! executes an external application nor opens an HTTP endpoint.
//!
//! Explicit report/attempt links establish historical actor ownership only.
//! Legacy rows do not contain current access grants or immutable file identities;
//! they cannot authorize a reducer transition or establish safe live parity.
use pitcrew_engine::observer::{self, Action, Activity, Actor, Diff, Safety};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, Row, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    collections::BTreeMap,
    fs::{self, File, Metadata},
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};

const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ROWS: i64 = 50_000;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidSourceKey,
    SourceUnavailable,
    SidecarPresent,
    SourceChanged,
    SourceTooLarge,
    SnapshotUnavailable,
    InvalidSchema,
    TooManyRows,
    InvalidRow,
    DestinationAliasesSource,
    StoreUnavailable,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never include source paths, SQLite error text, titles, or audit details.
        write!(f, "legacy snapshot import: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeEvidence {
    Ok,
    Bad,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyReplay {
    pub version: u8,
    pub source: String,
    pub report: u64,
    pub at: u64,
    /// Table name and primary key, scoped by `source`, never a release title.
    pub key: String,
    pub activity: Activity,
    /// Historical arr file ID is retained for review, never promoted into a
    /// domain FileIdentity: the external tool did not record the agent revision.
    pub decoded_arr_file: Option<u64>,
    pub decode: Option<DecodeEvidence>,
}
/// Compatibility alias for existing snapshot consumers.
pub type PythonReplay = LegacyReplay;

impl LegacyReplay {
    pub fn comparison(&self) -> Diff {
        observer::compare(
            format!("python:{}:report:{}", self.source, self.report),
            self.activity.clone(),
            Activity {
                action: Action::Hold,
                actor: Actor::Engine,
                // No exact job/file/access correlation for a reducer proposal.
                correlated: false,
                safety: Safety::default(),
            },
        )
    }
}

/// Counts are evidence of recorded spending, not proof of an atomic reservation
/// or attribution to any particular report. Global buckets have no report FK.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetEvidence {
    pub research_global_rows: u64,
    pub research_viewer_rows: u64,
    pub item_fix_rows: u64,
    pub decode_retry_rows: u64,
    pub owner_fix_rows: u64,
    pub landmine_rows: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    ResearchGlobal,
    ResearchViewer,
    Fix,
    DecodeRetry,
    Owner,
    Landmine,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetRecord {
    pub key: String,
    pub at: u64,
    pub kind: BudgetKind,
}
#[derive(Debug, Default)]
pub struct Snapshot {
    pub source: String,
    pub replays: Vec<LegacyReplay>,
    pub budget: BudgetEvidence,
    pub budget_records: Vec<BudgetRecord>,
    pub unowned_attempts: u64,
    pub uncorrelated_audits: u64,
}

/// Refuse aliased destinations before opening the writable engine Store.
pub fn distinct_destination(source: &Path, destination: &Path) -> Result<(), Error> {
    let source_path = fs::canonicalize(source).map_err(|_| Error::SourceUnavailable)?;
    let destination_path = match fs::symlink_metadata(destination) {
        // A dangling link exists as a directory entry even though exists()
        // follows it and returns false. Never treat that alias as a new file.
        Ok(_) => fs::canonicalize(destination).map_err(|_| Error::SourceUnavailable)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::canonicalize(
            destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .map_err(|_| Error::SourceUnavailable)?
        .join(
            destination
                .file_name()
                .ok_or(Error::DestinationAliasesSource)?,
        ),
        Err(_) => return Err(Error::SourceUnavailable),
    };
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut protected = source_path.as_os_str().to_owned();
        protected.push(suffix);
        if destination_path == Path::new(&protected) {
            return Err(Error::DestinationAliasesSource);
        }
    }
    let source = fs::metadata(source).map_err(|_| Error::SourceUnavailable)?;
    if let Ok(destination) = fs::metadata(destination)
        && source.dev() == destination.dev()
        && source.ino() == destination.ino()
    {
        return Err(Error::DestinationAliasesSource);
    }
    Ok(())
}
fn sidecars_absent(path: &Path) -> Result<(), Error> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        match fs::symlink_metadata(Path::new(&sidecar)) {
            Ok(_) => return Err(Error::SidecarPresent),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error::SourceUnavailable),
        }
    }
    Ok(())
}
fn same_version(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
fn read_bounded<R: Read>(source: &mut R, limit: u64) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    source
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::SourceUnavailable)?;
    if bytes.len() as u64 > limit {
        return Err(Error::SourceTooLarge);
    }
    Ok(bytes)
}
fn validate_capture(
    before: &Metadata,
    after: &Metadata,
    path_after: &Metadata,
    copied: u64,
) -> Result<(), Error> {
    if !same_version(before, after) || !same_version(before, path_after) || copied != before.len() {
        return Err(Error::SourceChanged);
    }
    Ok(())
}
fn capture(source: &Path) -> Result<tempfile::NamedTempFile, Error> {
    let path = fs::canonicalize(source).map_err(|_| Error::SourceUnavailable)?;
    sidecars_absent(&path)?;
    if !fs::metadata(&path)
        .map_err(|_| Error::SourceUnavailable)?
        .is_file()
    {
        return Err(Error::SourceUnavailable);
    }
    let mut source = File::open(&path).map_err(|_| Error::SourceUnavailable)?;
    let before = source.metadata().map_err(|_| Error::SourceUnavailable)?;
    if !before.is_file() {
        return Err(Error::SourceUnavailable);
    }
    if before.len() > MAX_BYTES {
        return Err(Error::SourceTooLarge);
    }
    let bytes = read_bounded(&mut source, MAX_BYTES)?;
    let after = source.metadata().map_err(|_| Error::SourceUnavailable)?;
    let path_after = fs::metadata(&path).map_err(|_| Error::SourceChanged)?;
    sidecars_absent(&path)?;
    validate_capture(&before, &after, &path_after, bytes.len() as u64)?;
    // SQLite only sees our private frozen copy. The source is opened by std as
    // read-only, so SQLite cannot create SHM/WAL or checkpoint it, even on errors.
    let mut snapshot = tempfile::NamedTempFile::new().map_err(|_| Error::SnapshotUnavailable)?;
    snapshot
        .write_all(&bytes)
        .map_err(|_| Error::SnapshotUnavailable)?;
    snapshot.flush().map_err(|_| Error::SnapshotUnavailable)?;
    Ok(snapshot)
}
fn positive(value: i64) -> Result<u64, Error> {
    u64::try_from(value)
        .ok()
        .filter(|v| *v > 0)
        .ok_or(Error::InvalidRow)
}
fn timestamp(ts: f64) -> Result<u64, Error> {
    if !ts.is_finite() || ts < 0.0 || ts >= i64::MAX as f64 {
        Err(Error::InvalidRow)
    } else {
        Ok(ts as u64)
    }
}
fn source_key(key: &str) -> Result<(), Error> {
    if key.is_empty()
        || key.len() > 64
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Err(Error::InvalidSourceKey)
    } else {
        Ok(())
    }
}
struct Report {
    item: String,
    instance: Option<String>,
    media: Option<i64>,
    media_type: String,
}
fn audit_report(detail: &str) -> Option<(u64, &str)> {
    let rest = detail.strip_prefix("report ")?;
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    let id = rest[..end].parse::<u64>().ok().filter(|id| *id > 0)?;
    Some((id, &rest[end..]))
}
fn replay(source: &str, report: u64, at: u64, key: String, action: Action) -> LegacyReplay {
    LegacyReplay {
        version: 1,
        source: source.into(),
        report,
        at,
        key,
        activity: Activity {
            action,
            actor: Actor::Legacy,
            correlated: true,
            safety: Safety::default(),
        },
        decoded_arr_file: None,
        decode: None,
    }
}

/// Accept an administrator-prepared standalone backup. WAL/SHM/journal sidecars
/// are rejected, including empty files: silently ignoring WAL loses committed
/// fixes. Never checkpoint or copy the active external database with this command.
pub async fn read(source: &Path, key: &str) -> Result<Snapshot, Error> {
    source_key(key)?;
    let snapshot = capture(source)?;
    let options = SqliteConnectOptions::new()
        .filename(snapshot.path())
        .read_only(true)
        .immutable(true)
        .pragma("query_only", "ON")
        .pragma("trusted_schema", "OFF");
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .map_err(|_| Error::InvalidSchema)?;
    let mut tx = connection.begin().await.map_err(|_| Error::InvalidSchema)?;
    let integrity: String = sqlx::query_scalar("PRAGMA quick_check(1)")
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| Error::InvalidSchema)?;
    if integrity != "ok" {
        return Err(Error::InvalidSchema);
    }
    let mut total = 0;
    for table in [
        "reports",
        "fix_attempts",
        "audit",
        "item_fixes",
        "rate_events",
    ] {
        let kind: Option<String> =
            sqlx::query_scalar("SELECT type FROM sqlite_schema WHERE name=?")
                .bind(table)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|_| Error::InvalidSchema)?;
        if kind.as_deref() != Some("table") {
            return Err(Error::InvalidSchema);
        }
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| Error::InvalidSchema)?;
        total += count;
        if total > MAX_ROWS {
            return Err(Error::TooManyRows);
        }
    }
    let mut reports = BTreeMap::new();
    for row in sqlx::query(
        "SELECT id,item_key,media_type,arr_instance,arr_media_id FROM reports ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|_| Error::InvalidSchema)?
    {
        let id = positive(row.try_get("id").map_err(|_| Error::InvalidRow)?)?;
        reports.insert(
            id,
            Report {
                item: row.try_get("item_key").map_err(|_| Error::InvalidRow)?,
                instance: row.try_get("arr_instance").map_err(|_| Error::InvalidRow)?,
                media: row.try_get("arr_media_id").map_err(|_| Error::InvalidRow)?,
                media_type: row.try_get("media_type").map_err(|_| Error::InvalidRow)?,
            },
        );
    }
    let mut result = Snapshot {
        source: key.into(),
        ..Snapshot::default()
    };
    let mut owned = std::collections::BTreeSet::new();
    for row in sqlx::query("SELECT id,report_id,item_key,instance,media_kind,media_id,verdict,ts,CASE WHEN length(title)>0 THEN 1 ELSE 0 END AS has_release FROM fix_attempts ORDER BY id")
        .fetch_all(&mut *tx).await.map_err(|_| Error::InvalidSchema)?
    {
        let id = positive(row.try_get("id").map_err(|_| Error::InvalidRow)?)?;
        let report_id: Option<i64> = row.try_get("report_id").map_err(|_| Error::InvalidRow)?;
        let report_id = report_id.and_then(|v| positive(v).ok());
        let instance: String = row.try_get("instance").map_err(|_| Error::InvalidRow)?;
        let item: String = row.try_get("item_key").map_err(|_| Error::InvalidRow)?;
        let kind: String = row.try_get("media_kind").map_err(|_| Error::InvalidRow)?;
        let media: i64 = row.try_get("media_id").map_err(|_| Error::InvalidRow)?;
        positive(media)?;
        let verdict: String = row.try_get("verdict").map_err(|_| Error::InvalidRow)?;
        let has_release: i64 = row.try_get("has_release").map_err(|_| Error::InvalidRow)?;
        let Some((report_id, report)) = report_id.and_then(|id| reports.get(&id).map(|report| (id, report))) else {
            result.unowned_attempts += 1;
            continue;
        };
        if report.item != item || report.instance.as_deref() != Some(instance.as_str())
            || report.media != Some(media)
            || !matches!((report.media_type.as_str(),kind.as_str()), ("movie","movie") | ("show","episode"))
            || !matches!(verdict.as_str(), "grabbed" | "imported" | "ok" | "bad") || has_release != 1
        {
            result.unowned_attempts += 1;
            continue;
        }
        let ts: f64 = row.try_get("ts").map_err(|_| Error::InvalidRow)?;
        let at = timestamp(ts)?;
        owned.insert(report_id);
        result.replays.push(replay(key, report_id, at, format!("attempt:{id}:grab"), Action::Grab));
    }
    // Parse only exact machine-generated report prefixes. A title, matching time,
    // or arr media ID alone is insufficient for ownership.
    for row in sqlx::query("SELECT id,action,target,outcome,detail,CAST(strftime('%s',ts) AS INTEGER) AS at FROM audit WHERE action IN ('replacement_manual_import','replacement_decode_check','dm_sent') ORDER BY id")
        .fetch_all(&mut *tx).await.map_err(|_| Error::InvalidSchema)?
    {
        let id = positive(row.try_get("id").map_err(|_| Error::InvalidRow)?)?;
        let action: String = row.try_get("action").map_err(|_| Error::InvalidRow)?;
        let target: Option<String> = row.try_get("target").map_err(|_| Error::InvalidRow)?;
        let outcome: Option<String> = row.try_get("outcome").map_err(|_| Error::InvalidRow)?;
        let detail: Option<String> = row.try_get("detail").map_err(|_| Error::InvalidRow)?;
        let Some((report_id, suffix)) = detail.as_deref().and_then(audit_report) else {
            result.uncorrelated_audits += 1;
            continue;
        };
        let Some(report) = reports.get(&report_id).filter(|_| owned.contains(&report_id)) else {
            result.uncorrelated_audits += 1;
            continue;
        };
        let at: Option<i64> = row.try_get("at").map_err(|_| Error::InvalidRow)?;
        let at = at.ok_or(Error::InvalidRow)?;
        let at = u64::try_from(at).map_err(|_| Error::InvalidRow)?;
        let mut observation = replay(key, report_id, at, format!("audit:{id}"), Action::Hold);
        let scoped = report.instance.as_ref().map(|instance| format!("{instance}: {}", report.item));
        match action.as_str() {
            "replacement_manual_import" if suffix.starts_with(": ") && target == scoped && outcome.as_deref() == Some("ok") => {
                observation.activity.action = Action::Import;
            }
            "replacement_decode_check" if target.as_deref() == Some(report.item.as_str()) => {
                let Some(file) = suffix.strip_prefix(" file ").and_then(|s| s.split_once(": ")).and_then(|(s, _)| s.parse::<u64>().ok()).filter(|id| *id > 0) else {
                    result.uncorrelated_audits += 1;
                    continue;
                };
                observation.activity.action = Action::Decode;
                observation.decoded_arr_file = Some(file);
                observation.decode = Some(match outcome.as_deref() {
                    Some("ok") => DecodeEvidence::Ok,
                    Some("bad") => DecodeEvidence::Bad,
                    _ => DecodeEvidence::Unknown,
                });
            }
            "dm_sent" if suffix.starts_with(" resolved: ") && target.as_deref() == Some(report.item.as_str()) && outcome.as_deref() == Some("ok") => {
                observation.activity.action = Action::Ready;
            }
            _ => {
                result.uncorrelated_audits += 1;
                continue;
            }
        }
        result.replays.push(observation);
    }
    for row in sqlx::query("SELECT id,key,ts FROM rate_events WHERE bucket='research' ORDER BY id")
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| Error::InvalidSchema)?
    {
        let id = positive(row.try_get("id").map_err(|_| Error::InvalidRow)?)?;
        let at = timestamp(row.try_get("ts").map_err(|_| Error::InvalidRow)?)?;
        let rate_key: String = row.try_get("key").map_err(|_| Error::InvalidRow)?;
        let kind = if rate_key == "global" {
            result.budget.research_global_rows += 1;
            BudgetKind::ResearchGlobal
        } else {
            result.budget.research_viewer_rows += 1;
            BudgetKind::ResearchViewer
        };
        result.budget_records.push(BudgetRecord {
            key: format!("rate:{id}"),
            at,
            kind,
        });
    }
    for row in sqlx::query("SELECT id,kind,ts FROM item_fixes ORDER BY id")
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| Error::InvalidSchema)?
    {
        let id = positive(row.try_get("id").map_err(|_| Error::InvalidRow)?)?;
        let at = timestamp(row.try_get("ts").map_err(|_| Error::InvalidRow)?)?;
        let kind: String = row.try_get("kind").map_err(|_| Error::InvalidRow)?;
        let kind = match kind.as_str() {
            "fix" => {
                result.budget.item_fix_rows += 1;
                BudgetKind::Fix
            }
            "decode_retry" => {
                result.budget.decode_retry_rows += 1;
                BudgetKind::DecodeRetry
            }
            "owner" => {
                result.budget.owner_fix_rows += 1;
                BudgetKind::Owner
            }
            "landmine" => {
                result.budget.landmine_rows += 1;
                BudgetKind::Landmine
            }
            _ => return Err(Error::InvalidRow),
        };
        result.budget_records.push(BudgetRecord {
            key: format!("item_fix:{id}"),
            at,
            kind,
        });
    }
    tx.rollback().await.map_err(|_| Error::InvalidSchema)?;
    connection.close().await.map_err(|_| Error::InvalidSchema)?;
    Ok(result)
}

impl crate::App {
    /// Import historical external-tool evidence without enabling effects.
    pub async fn import_legacy(&self, snapshot: Snapshot, at: u64) -> Result<usize, Error> {
        self.import_python(snapshot, at).await
    }

    /// Import is restart-safe: each stable table/row key deduplicates unchanged
    /// evidence. Changing an existing historical row is an explicit conflict.
    pub async fn import_python(&self, snapshot: Snapshot, at: u64) -> Result<usize, Error> {
        let metadata = serde_json::json!({
            "budget": snapshot.budget,
            "budget_attributed_to_report": false,
            "unowned_attempts": snapshot.unowned_attempts,
            "uncorrelated_audits": snapshot.uncorrelated_audits,
        });
        self.store
            .ingest_observation(
                &format!("python_import:{}", snapshot.source),
                &metadata.to_string(),
                &metadata,
                at,
            )
            .await
            .map_err(|_| Error::StoreUnavailable)?;
        for record in snapshot.budget_records {
            self.store
                .ingest_observation(
                    &format!("python_budget:{}", snapshot.source),
                    &record.key,
                    &serde_json::to_value(&record).map_err(|_| Error::InvalidRow)?,
                    record.at,
                )
                .await
                .map_err(|_| Error::StoreUnavailable)?;
        }
        let mut inserted = 0;
        for replay in snapshot.replays {
            inserted += usize::from(
                self.store
                    .ingest_observation(
                        &format!("python_replay:{}", replay.source),
                        &replay.key,
                        &serde_json::to_value(&replay).map_err(|_| Error::InvalidRow)?,
                        replay.at,
                    )
                    .await
                    .map_err(|_| Error::StoreUnavailable)?,
            );
        }
        Ok(inserted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reader_limits_growing_streams_and_preserves_short_reads() {
        use std::io::Cursor;
        assert_eq!(
            read_bounded(&mut Cursor::new(b"abcd"), 3),
            Err(Error::SourceTooLarge)
        );
        assert_eq!(
            read_bounded(&mut Cursor::new(b"abc"), 3),
            Ok(b"abc".to_vec())
        );
        assert_eq!(read_bounded(&mut Cursor::new(b"ab"), 3), Ok(b"ab".to_vec()));
        struct FailedRead;
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("synthetic read failure"))
            }
        }
        assert_eq!(
            read_bounded(&mut FailedRead, 3),
            Err(Error::SourceUnavailable)
        );
    }

    #[test]
    fn capture_rejects_open_file_changes_path_replacements_and_short_copies_independently() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"ab").unwrap();
        let before = file.as_file().metadata().unwrap();
        assert_eq!(validate_capture(&before, &before, &before, 2), Ok(()));
        file.as_file()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(123))
            .unwrap();
        let changed = file.as_file().metadata().unwrap();
        assert_eq!(
            validate_capture(&before, &changed, &before, 2),
            Err(Error::SourceChanged)
        );
        let mut replacement = tempfile::NamedTempFile::new().unwrap();
        replacement.write_all(b"ab").unwrap();
        let replaced = replacement.as_file().metadata().unwrap();
        assert_eq!(
            validate_capture(&before, &before, &replaced, 2),
            Err(Error::SourceChanged)
        );
        assert_eq!(
            validate_capture(&before, &before, &before, 1),
            Err(Error::SourceChanged)
        );
    }

    #[test]
    fn sidecar_lookup_errors_are_not_treated_as_absent_files() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("loop");
        std::os::unix::fs::symlink("loop", &link).unwrap();
        assert_eq!(
            sidecars_absent(&link.join("source")),
            Err(Error::SourceUnavailable)
        );
    }

    #[test]
    fn capture_version_detects_metadata_changes_without_changing_file_contents() {
        use std::os::unix::fs::PermissionsExt;
        let file = tempfile::NamedTempFile::new().unwrap();
        let before = file.as_file().metadata().unwrap();
        assert!(same_version(&before, &before));
        // Linux may coalesce timestamps within one filesystem clock tick.
        std::thread::sleep(std::time::Duration::from_millis(20));
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o400))
            .unwrap();
        let after = file.as_file().metadata().unwrap();
        assert!(!same_version(&before, &after));
    }

    #[test]
    fn timestamps_preserve_epoch_and_fractional_values_but_reject_non_finite_or_overflow() {
        for ts in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -0.1,
            i64::MAX as f64,
        ] {
            assert_eq!(timestamp(ts), Err(Error::InvalidRow));
        }
        assert_eq!(timestamp(0.0), Ok(0));
        assert_eq!(timestamp(1234.9), Ok(1234));
        assert_eq!(positive(0), Err(Error::InvalidRow));
        assert_eq!(positive(-1), Err(Error::InvalidRow));
        assert_eq!(positive(10), Ok(10));
    }

    #[test]
    fn audit_report_accepts_only_a_positive_decimal_id_with_a_machine_prefix() {
        assert_eq!(
            audit_report("report 12 resolved: done"),
            Some((12, " resolved: done"))
        );
        for text in [
            "report 0 resolved: done",
            "report -1 resolved: done",
            "report 12",
            "report twelve: done",
            "report 18446744073709551616: done",
            "prefix report 12: done",
        ] {
            assert_eq!(audit_report(text), None);
        }
    }
}
