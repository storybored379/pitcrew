use pitcrew::App;
mod init;
use pitcrew_arr::{Client, Kind, Page};
use pitcrew_config::{ArrKind, Config, EngineMode};
use pitcrew_engine::{
    observer,
    webhook::{ArrObservation, Kind as EventKind, Secret},
};
use pitcrew_store::{BudgetLimits, Store};
use pitcrew_upstream::{Auth, Endpoint, HttpTransport, Limits};
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    process::ExitCode,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("{}", json!({"event":"pitcrew_failed","reason":reason}));
            ExitCode::FAILURE
        }
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
async fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "init") {
        return init::run(&args[1..]).await;
    }
    if args.first().is_some_and(|arg| arg == "web") {
        let (path, check) = match &args[1..] {
            [flag, path] if flag == "--config" => (path, false),
            [flag, path, check] if flag == "--config" && check == "--check" => (path, true),
            _ => return Err("usage: pitcrew web --config FILE [--check]".into()),
        };
        let source = std::fs::read_to_string(path).map_err(|_| "config unreadable")?;
        let config = Config::parse(&source).map_err(|error| error.to_string())?;
        return pitcrew::stack::web(&config, check)
            .await
            .map_err(Into::into);
    }
    if args.first().is_some_and(|arg| arg == "doctor") {
        let (path, connectivity) = match &args[1..] {
            [flag, path] if flag == "--config" => (path, false),
            [flag, path, network] if flag == "--config" && network == "--connectivity" => {
                (path, true)
            }
            _ => return Err("usage: pitcrew doctor --config FILE [--connectivity]".into()),
        };
        let source = std::fs::read_to_string(path).map_err(|_| "config unreadable")?;
        let config = Config::parse(&source).map_err(|error| error.to_string())?;
        let transport = if connectivity {
            Some(HttpTransport::new().map_err(|error| error.to_string())?)
        } else {
            None
        };
        let report = pitcrew::doctor::inspect(
            &config,
            transport
                .as_ref()
                .map(|t| t as &dyn pitcrew_upstream::Transport),
        )
        .await;
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|_| "doctor report encoding failed")?
        );
        return if report.success() {
            Ok(())
        } else {
            Err("doctor checks failed".into())
        };
    }
    if args == ["--help"] {
        println!(
            "pitcrew --config FILE [--export JSONL | --import-legacy SQLITE --source-key KEY]\npitcrew init [OPTIONS] (see init --help)\npitcrew doctor --config FILE [--connectivity]\npitcrew web --config FILE [--check]\nShadow control plane. Doctor is offline unless --connectivity is supplied. See docs/stack.md."
        );
        return Ok(());
    }
    let (config_path, export, python_import) = match args.as_slice() {
        [flag, file] if flag == "--config" => (file, None, None),
        [flag, file, output, target] if flag == "--config" && output == "--export" => {
            (file, Some(target), None)
        }
        [flag, file, import, source, source_key, key]
            if flag == "--config" && ["--import-legacy", "--import-python"].contains(&import.as_str()) && source_key == "--source-key" =>
        {
            (file, None, Some((source, key)))
        }
        _ => return Err("usage: pitcrew --config FILE [--export JSONL | --import-legacy SQLITE --source-key KEY]".into()),
    };
    let source = std::fs::read_to_string(config_path).map_err(|_| "config unreadable")?;
    let config = Config::parse(&source).map_err(|e| e.to_string())?;
    // Offline composition is not permission to schedule live repairs. Resolve
    // every local prerequisite before explaining the remaining runtime refusal.
    if config.engine.mode == EngineMode::Live {
        let store = Store::open(&config.database)
            .await
            .map_err(|_| "live requires an available durable engine store")?;
        let validated = pitcrew::live::validate(&config, Some(store), now()).await?;
        let _composition = validated.compose(
            HttpTransport::new().map_err(|_| "live HTTP transport initialization failed")?,
            Arc::new(now),
            &pitcrew::live_delivery::HttpFactory,
        )?;
        eprintln!(
            "{}",
            json!({"event":"pitcrew_live_refused","mode":"live","effects_enabled":false,"reason":pitcrew::live::UNAVAILABLE})
        );
        return Err(pitcrew::live::UNAVAILABLE.into());
    }
    if let Some((source, key)) = python_import {
        // Entirely offline: do this before resolving secrets or building clients.
        pitcrew::legacy::distinct_destination(Path::new(source), &config.database)
            .map_err(|error| error.to_string())?;
        let snapshot = pitcrew::legacy::read(Path::new(source), key)
            .await
            .map_err(|error| error.to_string())?;
        let app = App::new(
            Store::open(&config.database)
                .await
                .map_err(|_| "database unavailable")?,
            vec![],
            None,
            BudgetLimits {
                global_hour: config.budgets.global_hour,
                global_day: config.budgets.global_day,
                viewer_hour: config.budgets.viewer_hour,
                viewer_day: config.budgets.viewer_day,
            },
            config.budgets.per_job,
            BTreeMap::new(),
        )
        .map_err(|_| "engine configuration")?;
        let imported = app
            .import_legacy(snapshot, now())
            .await
            .map_err(|error| error.to_string())?;
        let mut cursor = 0;
        loop {
            let next = app.project(cursor).await?;
            if next == cursor {
                break;
            }
            cursor = next;
        }
        println!(
            "{}",
            json!({"event":"legacy_snapshot_imported","new_actions":imported,"effects_enabled":false})
        );
        return Ok(());
    }
    let store = Store::open(&config.database)
        .await
        .map_err(|_| "database unavailable")?;
    let mut instances = vec![];
    for arr in &config.arr {
        if let Some(reference) = &arr.webhook_secret {
            let secret = reference
                .resolve("arr.webhook_secret")
                .map_err(|e| e.to_string())?;
            instances.push((
                arr.id.clone(),
                Secret::new(secret.expose()).map_err(|e| e.to_string())?,
            ));
        }
    }
    let control = config
        .engine
        .control_secret
        .as_ref()
        .map(|reference| {
            let secret = reference
                .resolve("engine.control_secret")
                .map_err(|e| e.to_string())?;
            Secret::new(secret.expose()).map_err(|e| e.to_string())
        })
        .transpose()?;
    let profiles = config
        .download_clients
        .iter()
        .map(|c| (c.id.clone(), c.effective_deadlines()))
        .collect::<BTreeMap<_, _>>();
    let app = App::new(
        store,
        instances,
        control,
        BudgetLimits {
            global_hour: config.budgets.global_hour,
            global_day: config.budgets.global_day,
            viewer_hour: config.budgets.viewer_hour,
            viewer_day: config.budgets.viewer_day,
        },
        config.budgets.per_job,
        profiles,
    )
    .map_err(|e| e.to_string())?;
    let app = if let Some(reference) = &config.engine.admin_secret {
        let secret = reference
            .resolve("engine.admin_secret")
            .map_err(|e| e.to_string())?;
        app.with_admin(Secret::new(secret.expose()).map_err(|e| e.to_string())?)
    } else {
        app
    };
    let app = Arc::new(app);
    if let Some(path) = export {
        let mut cursor = 0;
        loop {
            let next = app.project(cursor).await?;
            if next == cursor {
                break;
            }
            cursor = next;
        }
        let reports = app.reports().await?;
        let mut output = private_file(Path::new(path)).map_err(|_| "report file unavailable")?;
        for report in &reports {
            serde_json::to_writer(&mut output, report).map_err(|_| "report write failed")?;
            writeln!(output).map_err(|_| "report write failed")?;
        }
        let mut arbiter = private_file(Path::new(&format!("{path}.arbiter.jsonl")))
            .map_err(|_| "arbiter report file unavailable")?;
        let mut cursor = 0;
        let mut arbiter_count = 0;
        loop {
            let rows = app
                .store
                .observations_after(cursor, 1000)
                .await
                .map_err(|_| "report store unavailable")?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                cursor = row.id;
                if row.source.starts_with("arbiter:") {
                    serde_json::to_writer(&mut arbiter, &row)
                        .map_err(|_| "arbiter report write failed")?;
                    writeln!(arbiter).map_err(|_| "arbiter report write failed")?;
                    arbiter_count += 1;
                }
            }
        }
        let summary = format!(
            "{} {} arbiter observations exported separately.",
            observer::summary(&reports),
            arbiter_count
        );
        let mut human = private_file(Path::new(&format!("{path}.summary.txt")))
            .map_err(|_| "summary file unavailable")?;
        writeln!(human, "{summary}").map_err(|_| "summary write failed")?;
        println!("{summary}");
        return Ok(());
    }
    let bridge = pitcrew::stack::bridge(&config, app.store.clone()).await?;
    let mut runtime = pitcrew::runtime::Runtime::open(
        &config,
        app.store.clone(),
        HttpTransport::new().map_err(|e| e.to_string())?,
        Arc::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        }),
    )
    .await?;
    // A fresh install: the owner UI has not created its journal yet. Any other
    // journal or source problem still stops startup.
    let mut journal_pending = false;
    let mut consumer = match pitcrew::stack::admin_consumer(&config, bridge.as_ref()).await {
        Err(pitcrew::stack::JOURNAL_UNAVAILABLE)
            if config
                .admin
                .as_ref()
                .is_some_and(|admin| !admin.database.exists()) =>
        {
            journal_pending = true;
            None
        }
        opened => opened?,
    };
    let mut clients = vec![];
    if config.engine.poll_enabled {
        for arr in &config.arr {
            let key = arr
                .api_key
                .resolve("arr.api_key")
                .map_err(|e| e.to_string())?;
            let endpoint = Endpoint::new(
                &arr.url,
                Auth::arr(key.expose()).map_err(|e| e.to_string())?,
                Limits::default(),
            )
            .map_err(|e| e.to_string())?;
            let kind = match arr.kind {
                ArrKind::Sonarr => Kind::Sonarr,
                ArrKind::Radarr => Kind::Radarr,
            };
            clients.push((
                arr.id.clone(),
                kind,
                Client::new(
                    endpoint,
                    HttpTransport::new().map_err(|e| e.to_string())?,
                    kind,
                ),
            ));
        }
    }
    let listener = tokio::net::TcpListener::bind(config.engine.listen)
        .await
        .map_err(|_| "engine.listen: bind failed")?;
    eprintln!(
        "{}",
        json!({"event":"pitcrew_started","mode":"shadow","effects_enabled":false,"listen":listener.local_addr().map_err(|_| "listener unavailable")?.to_string(),"poll_enabled":config.engine.poll_enabled,"bridge_enabled":bridge.is_some(),"runtime_enabled":runtime.is_some()})
    );
    let mut tasks = tokio::task::JoinSet::new();
    let slots = Arc::new(tokio::sync::Semaphore::new(32));
    let worker_app = app.clone();
    let mut background =
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(config.engine.poll_seconds));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut cursor = 0;
            let mut last_sync = None;
            loop {
                timer.tick().await;
                if journal_pending {
                    match pitcrew::stack::admin_consumer(&config, bridge.as_ref()).await {
                        Ok(opened) => {
                            consumer = opened;
                            journal_pending = false;
                            eprintln!("{}", json!({"event":"admin_journal_connected"}));
                        }
                        Err(_) => eprintln!("{}", json!({"event":"admin_journal_waiting"})),
                    }
                }
                if let Some(runtime) = &mut runtime {
                    if last_sync.is_none_or(|at: u64| {
                        now().saturating_sub(at) >= config.runtime.sync_seconds
                    }) {
                        last_sync = Some(now());
                        // Reasons are fixed strings; they never carry upstream data or secrets.
                        let failure =
                            match tokio::time::timeout(Duration::from_secs(120), runtime.sync())
                                .await
                            {
                                Ok(Ok(())) => None,
                                Ok(Err(reason)) => Some(reason),
                                Err(_) => Some("catalog sync exceeded 120 seconds"),
                            };
                        let _ = runtime.health("catalog", failure.is_none()).await;
                        if let Some(reason) = failure {
                            eprintln!(
                                "{}",
                                json!({"event":"catalog_sync_incomplete","reason":reason})
                            );
                        }
                    }
                    match tokio::time::timeout(
                        Duration::from_secs(120),
                        runtime.tick_with_consumer(consumer.as_ref()),
                    )
                    .await
                    {
                        Ok(Ok(summary)) => {
                            eprintln!("{}", json!({"event":"runtime_tick","summary":summary}))
                        }
                        _ => eprintln!("{}", json!({"event":"runtime_tick_incomplete"})),
                    }
                }
                if runtime.is_none()
                    && let Some(consumer) = &consumer
                    && consumer.tick(now()).await.is_err()
                {
                    eprintln!("{}", json!({"event":"admin_consumer_failed"}));
                }
                for (id, kind, client) in &clients {
                    if !matches!(
                        tokio::time::timeout(
                            Duration::from_secs(30),
                            poll(&worker_app, id, *kind, client, now())
                        )
                        .await,
                        Ok(Ok(()))
                    ) {
                        eprintln!("{}", json!({"event":"poll_incomplete","instance":id}));
                    }
                }
                match worker_app.project(cursor).await {
                    Ok(next) => cursor = next,
                    Err(_) => eprintln!("{}", json!({"event":"projection_failed"})),
                }
                if let Some(bridge) = &bridge {
                    match bridge.tick(now()).await {
                        Ok(summary) => {
                            eprintln!("{}", json!({"event":"bridge_tick","summary":summary}));
                        }
                        Err(_) => eprintln!("{}", json!({"event":"bridge_tick_failed"})),
                    }
                }
            }
        });
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| "signal handler unavailable")?;
    loop {
        tokio::select! {
            result = &mut background => {
                return Err(if result.is_err() { "runtime task failed" } else { "runtime task stopped unexpectedly" }.into());
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|_| "accept failed")?;
                if let Ok(slot) = slots.clone().try_acquire_owned() { let app = app.clone(); tasks.spawn(async move { let _slot = slot; let _ = pitcrew::connection(app, stream, now()).await; }); }
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    background.abort();
    let _ = background.await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Bounded overlapping reads: stable history IDs deduplicate across polling and
/// restarts. History is newest first, so a page holding a row an earlier poll
/// stored closes the gap since that poll. Hitting the page cap without reaching
/// such a row or the end reports incomplete; absence never means failed.
async fn poll<T: pitcrew_upstream::Transport>(
    app: &App,
    id: &str,
    kind: Kind,
    client: &Client<T>,
    at: u64,
) -> Result<(), String> {
    let mut complete = false;
    for number in 1..=10 {
        let history = client
            .history(Page { number, size: 100 })
            .await
            .map_err(|e| e.to_string())?;
        if history.records.len() > 100 {
            return Err("history page exceeds requested bound".into());
        }
        let mut overlap = false;
        for row in history.records {
            let event = match row.event_type.as_str() {
                "grabbed" => EventKind::Grab,
                "downloadFolderImported" => EventKind::Download,
                "downloadFailed" => EventKind::DownloadFailed,
                "episodeFileDeleted" => EventKind::EpisodeFileDelete,
                "movieFileDeleted" => EventKind::MovieFileDelete,
                _ => continue,
            };
            let observation = ArrObservation {
                kind: event,
                download: row.download_id,
                media: row.episode_id.or(row.movie_id).into_iter().collect(),
                files: vec![],
            };
            let fresh = app
                .store
                .ingest_observation(
                    &format!("history:{id}"),
                    &row.id.to_string(),
                    &serde_json::to_value(observation).map_err(|_| "encoding")?,
                    at,
                )
                .await
                .map_err(|_| "store")?;
            overlap |= !fresh;
        }
        if overlap || u64::from(number) * 100 >= history.total_records {
            complete = true;
            break;
        }
    }
    let queue = client
        .queue(Page::default())
        .await
        .map_err(|e| e.to_string())?;
    if queue.total_records > 100 || queue.records.len() > 100 {
        complete = false;
    }
    let mut candidate_requests = 0;
    for row in queue.records.into_iter().take(100) {
        let candidates = if row.tracked_download_state.as_deref() == Some("importBlocked") {
            if let Some(download) = &row.download_id {
                if candidate_requests < 10 {
                    candidate_requests += 1;
                    client.manual_import_candidates(download).await.ok()
                } else {
                    complete = false;
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let observation = pitcrew_arr::shadow::observe_queue(
            kind,
            pitcrew_arr::shadow::Correlation {
                job: 0,
                library: 0,
                download: row.id,
                observed_at: at,
            },
            &row,
            candidates.as_deref(),
        );
        let report = pitcrew_arbiter::shadow::compare(&observation);
        app.store
            .ingest_observation(
                &format!("arbiter:{id}"),
                &format!("{}:{at}", row.id),
                &serde_json::to_value(report).map_err(|_| "encoding")?,
                at,
            )
            .await
            .map_err(|_| "store")?;
    }
    if complete {
        Ok(())
    } else {
        Err("poll window incomplete".into())
    }
}

#[cfg(test)]
#[path = "poll_tests.rs"]
mod poll_tests;
