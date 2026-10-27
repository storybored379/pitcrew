//! Explicit platform or local drill process. The loopback test mode never starts
//! platform runners. Operators must certify integrations before deployment.
use pitcrew_bot::server::{TestConfig, TestServer, router};
use std::{process::ExitCode, sync::Arc};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        println!(
            "pitcrew-bot --config FILE [--check]\npitcrew-bot (test-channel|run) LEGACY.json\n--check resolves local configuration and secrets without opening platform connections."
        );
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--config") {
        let (path, check) = match args.as_slice() {
            [_, path] => (path, false),
            [_, path, check] if check == "--check" => (path, true),
            _ => return Err("usage: pitcrew-bot --config FILE [--check]".into()),
        };
        let source = std::fs::read_to_string(path).map_err(|_| "could not read stack config")?;
        let config = pitcrew_config::Config::parse(&source).map_err(|error| error.to_string())?;
        let runtime = pitcrew_bot::stack::runtime(&config)?;
        if check {
            println!("bot configuration valid (upstreams not contacted)");
            return Ok(());
        }
        return pitcrew_bot::runtime::run(runtime).await.map_err(Into::into);
    }
    let [mode, path] = args.as_slice() else {
        return Err("usage: pitcrew-bot (test-channel|run) CONFIG.json".into());
    };
    if mode == "run" {
        let config =
            serde_json::from_slice(&std::fs::read(path).map_err(|_| "could not read bot config")?)
                .map_err(|_| "invalid bot config")?;
        return pitcrew_bot::runtime::run(config).await.map_err(Into::into);
    }
    if mode != "test-channel" {
        return Err("unknown bot mode; use test-channel or run".into());
    }
    let config: TestConfig =
        serde_json::from_slice(&std::fs::read(path).map_err(|_| "could not read test config")?)
            .map_err(|_| "invalid test config")?;
    let bind = config.bind;
    let server = TestServer::open(config).await?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|_| "could not bind loopback test channel")?;
    eprintln!(
        "Pit Crew authenticated synthetic test channel listening on {}",
        listener
            .local_addr()
            .map_err(|_| "listener address unavailable")?
    );
    axum::serve(listener, router(Arc::new(server)))
        .await
        .map_err(|_| "test channel listener failed".into())
}
