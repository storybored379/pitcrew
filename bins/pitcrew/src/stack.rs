//! Bootstrap adapters for the single shared TOML configuration.
use pitcrew_config::Config;
use std::sync::Arc;

pub fn bridge_options(config: &Config) -> pitcrew_bridge::Options {
    pitcrew_bridge::Options {
        policy: pitcrew_domain::Policy {
            max_grabs: config.budgets.per_job,
            stage_timeout: 180,
            total_timeout: 3600,
        },
        timeouts: pitcrew_domain::StageTimeouts {
            download: 180,
            import: 120,
            decode: 180,
            verify_retry: 180,
        },
        budgets: pitcrew_store::BudgetLimits {
            global_hour: config.budgets.global_hour,
            global_day: config.budgets.global_day,
            viewer_hour: config.budgets.viewer_hour,
            viewer_day: config.budgets.viewer_day,
        },
        cooldown_seconds: config.budgets.item_cooldown_seconds,
        evidence_max_age: config.stack.evidence_max_age_seconds,
    }
}

pub async fn bridge(
    config: &Config,
    store: pitcrew_store::Store,
) -> Result<Option<pitcrew_bridge::Bridge>, &'static str> {
    if !config.stack.enabled {
        return Ok(None);
    }
    pitcrew_bridge::Bridge::open(&config.stack.intake_database, store, bridge_options(config))
        .await
        .map(Some)
        .map_err(|_| "stack bridge could not open intake store")
}

pub fn web_config(config: &Config) -> Result<pitcrew_web::Config, &'static str> {
    let plex = config
        .plex
        .as_ref()
        .ok_or("viewer web requires plex configuration")?;
    Ok(pitcrew_web::Config {
        database: config
            .stack
            .intake_database
            .to_str()
            .ok_or("intake database path must be UTF-8")?
            .into(),
        bind: config.stack.web_listen,
        public_origin: config.stack.web_public_origin.clone(),
        plex_auth_url: config.stack.plex_auth_url.clone(),
        plex_server_url: plex.url.clone(),
        plex_server_id: plex.server_id.clone(),
        client_id: config.stack.plex_client_id.clone(),
        catalog: config
            .stack
            .catalog
            .iter()
            .map(|item| pitcrew_intake::CatalogItem {
                id: item.id.clone(),
                title: item.title.clone(),
                aliases: item.aliases.clone(),
                library_id: item.library_id.clone(),
                library_name: item.library_name.clone(),
                resolution: match item.resolution {
                    pitcrew_config::StackResolution::Hd => pitcrew_intake::Resolution::Hd,
                    pitcrew_config::StackResolution::Uhd => pitcrew_intake::Resolution::Uhd,
                },
                season: item.season,
                episode: item.episode,
            })
            .collect(),
        catalog_snapshot_file: match &config.stack.catalog_snapshot_file {
            Some(path) => Some(
                path.to_str()
                    .ok_or("catalog snapshot path must be UTF-8")?
                    .into(),
            ),
            None if config.stack.enabled => Some(
                crate::runtime::snapshot_path(config)
                    .to_str()
                    .ok_or("catalog snapshot path must be UTF-8")?
                    .into(),
            ),
            None => None,
        },
    })
}

/// Runs the pitcrew-web application; no process-specific config file is needed.
pub async fn web(config: &Config, check: bool) -> Result<(), &'static str> {
    let config = web_config(config)?;
    let auth = pitcrew_web::auth::PlexClient::new(
        &config.plex_auth_url,
        &config.plex_server_url,
        &config.plex_server_id,
        &config.client_id,
    )
    .map_err(|_| "invalid Plex configuration")?;
    if check {
        println!("viewer web configuration valid (upstreams not contacted)");
        return Ok(());
    }
    let store = pitcrew_intake_store::Store::open(&config.database)
        .await
        .map_err(|_| "intake database unavailable")?;
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|_| "web listener unavailable")?;
    eprintln!(
        "{}",
        serde_json::json!({
            "event": "pitcrew_web_started",
            "listen": listener.local_addr().map_err(|_| "web listener address unavailable")?.to_string()
        })
    );
    let app = Arc::new(pitcrew_web::App::new(
        store,
        Arc::new(auth),
        config,
        Arc::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        }),
    ));
    axum::serve(listener, app.router())
        .await
        .map_err(|_| "web listener failed")
}

/// The owner UI creates the journal, and it needs the controller's databases
/// to start, so on a fresh install the controller keeps retrying this one.
pub const JOURNAL_UNAVAILABLE: &str = "admin command journal unavailable; waiting for the owner UI";

/// Connect the owner command journal to the same engine/intake pair used by the
/// controller. Numerical report/job IDs alone do not identify an installation.
pub async fn admin_consumer(
    config: &Config,
    bridge: Option<&pitcrew_bridge::Bridge>,
) -> Result<Option<pitcrew_bridge::admin::Consumer>, &'static str> {
    let (Some(admin), Some(bridge)) = (&config.admin, bridge) else {
        return Ok(None);
    };
    for (configured, actual, error) in [
        (
            &admin.engine_database,
            &config.database,
            "admin.engine_database must match the control-plane engine database",
        ),
        (
            &admin.intake_database,
            &config.stack.intake_database,
            "admin.intake_database must match the control-plane intake database",
        ),
    ] {
        let configured = std::fs::canonicalize(configured).map_err(|_| error)?;
        let actual = std::fs::canonicalize(actual).map_err(|_| error)?;
        if configured != actual {
            return Err(error);
        }
    }
    pitcrew_bridge::admin::Consumer::open(
        &admin.database,
        bridge.clone(),
        admin.admin_account_ids.clone(),
    )
    .await
    .map(Some)
    .map_err(|_| JOURNAL_UNAVAILABLE)
}
