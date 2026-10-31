use pitcrew_bot::stack::runtime;
use pitcrew_config::Config;

fn source(path: &std::path::Path) -> String {
    format!(
        r#"
version=1
[stack]
intake_database='/state/intake.sqlite'
plex_client_id='synthetic-client'
bot_seerr_user_id=42
[[stack.catalog]]
id='item-hd'
title='Synthetic Show'
aliases=['Synthetic']
library_id='library'
library_name='Series'
resolution='hd'
season=1
episode=2
[[stack.catalog]]
id='item-uhd'
title='Synthetic Film'
library_id='library-uhd'
library_name='Films'
resolution='uhd'
[plex]
url='https://plex.invalid'
owner_token={{file={path:?}}}
server_id='synthetic-server'
[[channels]]
id='discord'
adapter={{kind='discord',token={{file={path:?}}},control_plane_url='http://127.0.0.1:7400',control_plane_secret={{file={path:?}}}}}
[[channels]]
id='telegram'
adapter={{kind='telegram',token={{file={path:?}}},control_plane_url='http://127.0.0.1:7400',control_plane_secret={{file={path:?}}}}}
[[channels]]
id='seerr'
adapter={{kind='seerr',url='https://seerr.invalid',api_key={{file={path:?}}}}}
"#,
        path = path.to_str().unwrap()
    )
}

#[test]
fn shared_config_resolves_secret_refs_and_maps_all_runtime_fields() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("token");
    std::fs::write(&path, "synthetic-token\n").unwrap();
    let parsed = Config::parse(&source(&path)).unwrap();
    let runtime = runtime(&parsed).unwrap();
    assert_eq!(runtime.database.to_str(), Some("/state/intake.sqlite"));
    assert_eq!(runtime.plex_auth_url, "https://plex.tv/");
    assert_eq!(runtime.plex_server_url, "https://plex.invalid");
    assert_eq!(runtime.plex_server_id, "synthetic-server");
    assert_eq!(runtime.client_id, "synthetic-client");
    assert_eq!(
        runtime.catalog,
        vec![
            pitcrew_intake::CatalogItem {
                id: "item-hd".into(),
                title: "Synthetic Show".into(),
                aliases: vec!["Synthetic".into()],
                library_id: "library".into(),
                library_name: "Series".into(),
                resolution: pitcrew_intake::Resolution::Hd,
                season: Some(1),
                episode: Some(2)
            },
            pitcrew_intake::CatalogItem {
                id: "item-uhd".into(),
                title: "Synthetic Film".into(),
                aliases: vec![],
                library_id: "library-uhd".into(),
                library_name: "Films".into(),
                resolution: pitcrew_intake::Resolution::Uhd,
                season: None,
                episode: None
            }
        ]
    );
    let discord = runtime.discord.unwrap();
    assert_eq!(discord.channel_id, "discord");
    assert_eq!(discord.bot_token, "synthetic-token");
    let telegram = runtime.telegram.unwrap();
    assert_eq!(telegram.channel_id, "telegram");
    assert_eq!(telegram.bot_token, "synthetic-token");
    let seerr = runtime.seerr.unwrap();
    assert_eq!(seerr.channel_id, "seerr");
    assert_eq!(seerr.api_url, "https://seerr.invalid");
    assert_eq!(seerr.api_key, "synthetic-token");
    assert_eq!(seerr.bot_user_id, 42);
}

#[test]
fn shared_config_fails_closed_on_missing_channels_secrets_and_bot_identity() {
    assert_eq!(
        runtime(&Config::parse("version=1").unwrap()).err(),
        Some("bot requires plex configuration")
    );
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("token");
    assert_eq!(
        runtime(&Config::parse(&source(&path)).unwrap()).err(),
        Some("bot token source unavailable")
    );
    std::fs::write(&path, "synthetic-token").unwrap();
    assert_eq!(
        runtime(&Config::parse(&source(&path).replace("bot_seerr_user_id=42", "")).unwrap()).err(),
        Some("Seerr requires stack.bot_seerr_user_id")
    );
    for (kind, suffix, error) in [
        (
            "discord",
            "token={file=PATH},control_plane_url='http://127.0.0.1:7400',control_plane_secret={file=PATH}",
            "bot supports one Discord channel instance",
        ),
        (
            "telegram",
            "token={file=PATH},control_plane_url='http://127.0.0.1:7400',control_plane_secret={file=PATH}",
            "bot supports one Telegram channel instance",
        ),
        (
            "seerr",
            "url='https://seerr.invalid',api_key={file=PATH}",
            "bot supports one Seerr channel instance",
        ),
        (
            "matrix",
            "homeserver_url='https://matrix.invalid',access_token={file=PATH}",
            "configured bot adapter has no implemented runtime",
        ),
    ] {
        let suffix = suffix.replace("PATH", &format!("{:?}", path.to_str().unwrap()));
        let config = Config::parse(&format!(
            "{}\n[[channels]]\nid='extra'\nadapter={{kind='{kind}',{suffix}}}",
            source(&path)
        ))
        .unwrap();
        assert_eq!(runtime(&config).err(), Some(error));
    }
}

#[tokio::test]
async fn shared_config_cli_check_is_offline_and_does_not_create_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("token");
    std::fs::write(&path, "synthetic-token").unwrap();
    let config = temp.path().join("stack.toml");
    // Even a mutation that ignores --check can reach only this owned fake
    // socket, never a platform endpoint. The child has a bounded lifetime.
    let fake = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source = source(&path);
    let local_only = format!(
        "{}\n[[channels]]\nid='seerr'\nadapter={{kind='seerr',url='http://{}',api_key={{file={:?}}}}}",
        source.split("[[channels]]").next().unwrap(),
        fake.local_addr().unwrap(),
        path.to_str().unwrap()
    );
    std::fs::write(
        &config,
        local_only.replace("/state/intake.sqlite", "must-not-exist.sqlite"),
    )
    .unwrap();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew-bot"))
            .current_dir(temp.path())
            .args(["--config"])
            .arg(config)
            .arg("--check")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!temp.path().join("must-not-exist.sqlite").exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-token"));
}

#[tokio::test]
async fn shared_config_cli_rejects_missing_paths_and_unknown_check_flags() {
    let temp = tempfile::tempdir().unwrap();
    for args in [
        vec!["--config"],
        vec!["--config", "missing.toml", "--unknown"],
        vec!["--config", "missing.toml", "--check", "extra"],
    ] {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew-bot"))
                .current_dir(temp.path())
                .args(args)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("usage: pitcrew-bot --config FILE [--check]")
        );
    }
    // A bare --config FILE is the normal run form: it reads the file first.
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_pitcrew-bot"))
            .current_dir(temp.path())
            .args(["--config", "missing.toml"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "could not read stack config"
    );
}
