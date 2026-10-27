use pitcrew_admin::{
    actions::HttpAdminApi, auth::PlexAuth, config, journal::Journal, read::Sources, web::App,
};
use std::{
    process::ExitCode,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
async fn run() -> Result<(), &'static str> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        println!(
            "pitcrew-admin --config FILE [--check]\nOwner-only Plex sign-in, read-only journals, audited admin requests. No automatic upstream probes."
        );
        return Ok(());
    }
    let (path, check) = match args.as_slice() {
        [flag, path] if flag == "--config" => (path, false),
        [flag, path, check] if flag == "--config" && check == "--check" => (path, true),
        _ => return Err("usage: pitcrew-admin --config FILE [--check]"),
    };
    let source = std::fs::read_to_string(path).map_err(|_| "configuration unreadable")?;
    let config = config::parse(&source).map_err(|_| "invalid admin configuration")?;
    if check {
        println!(
            "admin configuration valid (no secrets resolved, databases opened or upstreams contacted)"
        );
        return Ok(());
    }
    config::protect_sources(&config).map_err(
        |_| "admin database must be separate from both read-only sources and their SQLite sidecars",
    )?;
    let auth = PlexAuth::new(&config.plex_auth_url, &config.client_id)
        .map_err(|_| "invalid Plex configuration")?;
    let api = if let Some(api) = &config.api {
        let secret = api
            .secret
            .resolve("admin.api.secret")
            .map_err(|_| "admin API secret unavailable")?;
        Some(Arc::new(
            HttpAdminApi::new(&api.url, secret.expose())
                .map_err(|_| "invalid admin API configuration")?,
        ) as Arc<dyn pitcrew_admin::actions::AdminApi>)
    } else {
        None
    };
    let sources = Sources::open(&config.engine_database, &config.intake_database)
        .await
        .map_err(|_| "read-only engine/intake databases unavailable")?;
    let journal = Journal::open(&config.database)
        .await
        .map_err(|_| "admin journal unavailable")?;
    let now = Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    });
    journal
        .recover(now())
        .await
        .map_err(|_| "admin recovery failed")?;
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|_| "admin listener unavailable")?;
    let app = Arc::new(
        App::new(journal, sources, Arc::new(auth), api, config, now)
            .map_err(|_| "invalid admin configuration")?,
    );
    axum::serve(listener, app.router())
        .await
        .map_err(|_| "admin listener failed")
}
