use pitcrew_intake_store::Store;
use pitcrew_web::{App, Config, auth::PlexClient};
use std::{
    process::ExitCode,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}
async fn run() -> Result<(), &'static str> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        println!(
            "pitcrew-web --config FILE [--check]\nServer-rendered Plex intake; reports remain awaiting review. No repair effects."
        );
        return Ok(());
    }
    let (path, check) = match args.as_slice() {
        [flag, path] if flag == "--config" => (path, false),
        [flag, path, check] if flag == "--config" && check == "--check" => (path, true),
        _ => return Err("usage: pitcrew-web --config FILE [--check]"),
    };
    let source = std::fs::read_to_string(path).map_err(|_| "configuration unreadable")?;
    let config = Config::parse(&source)?;
    let auth = PlexClient::new(
        &config.plex_auth_url,
        &config.plex_server_url,
        &config.plex_server_id,
        &config.client_id,
    )
    .map_err(|_| "invalid Plex configuration")?;
    if check {
        println!("web configuration valid (upstreams not contacted)");
        return Ok(());
    }
    let store = Store::open(&config.database)
        .await
        .map_err(|_| "intake database unavailable")?;
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|_| "web listener unavailable")?;
    let app = Arc::new(App::new(
        store,
        Arc::new(auth),
        config,
        Arc::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        }),
    ));
    axum::serve(listener, app.router())
        .await
        .map_err(|_| "web listener failed")
}
