//! Local-only media agent. All operational jobs remain disabled.
use pitcrew_agent_core::{
    http::{App, Auth, serve},
    media::{MediaAgent, MediaConfig},
    plex_logs::{PlexLogConfig, PlexLogReader},
    runtime::LocalInspector,
};
use std::{path::Path, process::ExitCode, sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({"event": "agent_failed", "reason": error})
            );
            ExitCode::FAILURE
        }
    }
}
async fn run() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let filename = match args.as_slice() {
        [flag] if flag == "--help" => {
            println!(
                "pitcrew-agent --config FILE\nLinux media diagnostics; ffmpeg and ffprobe required. Local HTTP, Bearer authentication. See docs/agent.md."
            );
            return Ok(());
        }
        [flag, file] if flag == "--config" => file,
        _ => return Err("usage: pitcrew-agent --config FILE".into()),
    };
    let source = std::fs::read_to_string(Path::new(filename)).map_err(|_| "config unreadable")?;
    let config = pitcrew_config::Config::parse(&source).map_err(|error| error.to_string())?;
    let agent = config.agent.ok_or("agent configuration required")?;
    let secret = agent
        .shared_secret
        .resolve("agent.shared_secret")
        .map_err(|error| error.to_string())?;
    let auth = Auth::new(secret.expose())
        .map_err(|_| "agent.shared_secret: must contain 32–4096 visible ASCII bytes")?;
    let server = agent.server;
    let media = MediaAgent::new(MediaConfig {
        allowed_roots: agent.media_roots,
        storage_roots: agent.storage_roots,
        filesystem_timeout: Duration::from_millis(
            agent
                .checks
                .symlink_timeout_ms
                .min(agent.checks.mount_timeout_ms),
        ),
        read_timeout: Duration::from_millis(agent.checks.read_timeout_ms),
        header_timeout: Duration::from_secs(agent.checks.header_timeout_seconds),
        process_timeout: Duration::from_secs(server.decode_timeout_seconds),
        symlink_hop_limit: agent.checks.symlink_hop_limit as usize,
        slow_read_threshold: Duration::from_millis(agent.checks.slow_first_byte_ms),
        slow_bytes_per_second: agent.checks.slow_bytes_per_second,
        read_offsets_bytes: agent.checks.read_offsets_bytes,
        read_bytes: agent
            .checks
            .read_bytes
            .try_into()
            .map_err(|_| "invalid read bytes")?,
        decode_seconds: server.decode_seconds,
        decode_offsets_percent: server.decode_offsets_percent,
        max_requests: agent.decode_concurrency.into(),
        max_filesystem_workers: server.filesystem_workers,
        ffmpeg: server.ffmpeg,
        ffprobe: server.ffprobe,
    })
    .map_err(|_| "invalid media settings")?;
    let logs = server
        .plex_log_directory
        .map(|directory| {
            PlexLogReader::new(PlexLogConfig {
                directory,
                timezone: agent.plex_log_timezone,
                ..PlexLogConfig::default()
            })
        })
        .transpose()
        .map_err(|_| "invalid log settings")?;
    let media_control = media.clone();
    let inspector = LocalInspector::new(media, logs, Duration::from_secs(5));
    let app = Arc::new(
        App::new(
            auth,
            server.compatibility,
            Arc::new(inspector),
            server.requests_per_minute,
        )
        .map_err(|_| "invalid HTTP settings")?,
    );
    let listener = tokio::net::TcpListener::bind(server.listen)
        .await
        .map_err(|_| "listener unavailable")?;
    eprintln!("{{\"event\":\"agent_started\",\"api_version\":1}}");
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| "signal handler unavailable")?;
    let result = tokio::select! {
        result=serve(listener,app,server.max_connections,Duration::from_secs(server.request_timeout_seconds))=>result.map_err(|_|"listener failed"),
        _=tokio::signal::ctrl_c()=>Ok(()),
        _=terminate.recv()=>Ok(()),
    };
    if !media_control.shutdown(Duration::from_secs(5)).await {
        return Err("process cleanup deadline exceeded".into());
    }
    result.map_err(str::to_owned)
}
