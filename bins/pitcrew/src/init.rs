//! Additive setup adapter; reuse the same doctor as the standalone command.
use pitcrew_setup::{Discover, Options, Section, Server, Terminal, USAGE};
use pitcrew_upstream::{Auth, Endpoint, HttpTransport, Limits};
use std::time::Duration;

/// `--discover`: two bounded, authenticated, read-only Plex GETs.
struct PlexDiscovery;
impl Discover for PlexDiscovery {
    fn discover(&mut self, plex_url: &str, token: &str) -> pitcrew_setup::Result<Server> {
        let auth = Auth::plex(token).map_err(|_| "init: Plex token rejected")?;
        let limits = Limits {
            timeout: Duration::from_secs(15),
            max_response_bytes: 1024 * 1024,
        };
        let endpoint =
            Endpoint::new(plex_url, auth, limits).map_err(|_| "init: Plex URL rejected")?;
        let transport = HttpTransport::new().map_err(|_| "init: discovery client unavailable")?;
        let client = pitcrew_plex::Client::new(endpoint, transport);
        let handle = tokio::runtime::Handle::current();
        tokio::task::block_in_place(|| {
            handle.block_on(async {
                let id = client.identity().await.map_err(
                    |_| "init: Plex identity request failed; check the URL and owner token",
                )?;
                let sections = client
                    .sections()
                    .await
                    .map_err(|_| "init: Plex library list request failed")?;
                Ok(Server {
                    id,
                    sections: sections
                        .into_iter()
                        .map(|section| Section {
                            key: section.key,
                            kind: section.kind,
                            title: section.title.unwrap_or_default(),
                        })
                        .collect(),
                })
            })
        })
    }
}

pub async fn run(args: &[String]) -> Result<(), String> {
    let Some(options) = Options::parse(args)? else {
        println!("{USAGE}");
        return Ok(());
    };
    let installation = pitcrew_setup::create_with(options, &mut Terminal, &mut PlexDiscovery)?;
    let transport = if installation.connectivity {
        Some(
            pitcrew_upstream::HttpTransport::new()
                .map_err(|_| "init: connectivity client unavailable")?,
        )
    } else {
        None
    };
    let report = pitcrew::doctor::inspect(
        &installation.local_config,
        transport
            .as_ref()
            .map(|transport| transport as &dyn pitcrew_upstream::Transport),
    )
    .await;
    println!(
        "{}",
        serde_json::to_string(&report).map_err(|_| "init: doctor report encoding failed")?
    );
    if !report.success() {
        return Err("init: files saved, but doctor checks failed; correct the configuration before starting".into());
    }
    println!(
        "Created pitcrew.toml, secrets and state in the requested output directory. Doctor passed. Shadow mode is configured; live repair is not certified."
    );
    Ok(())
}
