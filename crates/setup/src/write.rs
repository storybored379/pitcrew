use crate::{Discover, Installation, Options, Prompt, Result, Server};
use pitcrew_config::Config;
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub(super) fn create(
    mut options: Options,
    prompt: &mut impl Prompt,
    discover: Option<&mut dyn Discover>,
) -> Result<Installation> {
    let output = PathBuf::from(
        options
            .values
            .remove("--output")
            .unwrap_or_else(|| "runtime".into()),
    );
    let output = std::path::absolute(output).map_err(|_| "init: output path unavailable")?;
    let runtime = options
        .values
        .remove("--runtime-root")
        .map(PathBuf::from)
        .unwrap_or_else(|| output.clone());
    if !runtime.is_absolute() {
        return Err("init: --runtime-root must be an absolute path");
    }
    if output
        .try_exists()
        .map_err(|_| "init: output path unavailable")?
    {
        return Err("init: output directory already exists; choose a new directory");
    }
    let plex_url = required(&mut options, prompt, "--plex-url", "Plex URL")?;
    if url::Url::parse(&plex_url)
        .map_err(|_| "init: Plex URL must be a valid HTTP(S) server root")?
        .path()
        != "/"
    {
        return Err("init: Plex URL must use the server root without a path prefix");
    }
    // Discovery needs the token first; it is read once and reused below.
    let mut plex_token = None;
    let server = if options.discover {
        let discover = discover.ok_or("init: discovery is unavailable in this build")?;
        let token = credential(
            &mut options,
            prompt,
            "--plex-token",
            "Plex owner token (hidden)",
        )?;
        let server = discover.discover(&plex_url, &token)?;
        plex_token = Some(token);
        Some(server)
    } else {
        None
    };
    let server_id = match (&server, options.values.remove("--plex-server-id")) {
        (Some(server), Some(given)) if given != server.id => {
            return Err("init: --plex-server-id does not match the discovered server");
        }
        (Some(server), _) => server.id.clone(),
        (None, Some(given)) => given,
        (None, None) => required(
            &mut options,
            prompt,
            "--plex-server-id",
            "Plex server machineIdentifier",
        )?,
    };
    let kind = required(
        &mut options,
        prompt,
        "--arr-kind",
        "Library manager (sonarr or radarr)",
    )?;
    if !matches!(kind.as_str(), "sonarr" | "radarr") {
        return Err("init: --arr-kind must be sonarr or radarr");
    }
    let arr_url = required(&mut options, prompt, "--arr-url", "Library manager URL")?;
    if options.libraries.is_empty() {
        let label = match &server {
            Some(server) => format!(
                "Plex library section key for this manager ({})",
                section_list(server)
            ),
            None => "Plex library section key for this manager".into(),
        };
        let library = required(&mut options, prompt, "--library", &label)?;
        options.libraries.push(library);
    }
    if let Some(server) = &server
        && !options
            .libraries
            .iter()
            .all(|key| server.sections.iter().any(|section| section.key == *key))
    {
        return Err("init: library key is not a section on the discovered server");
    }
    let origin = options
        .values
        .remove("--public-origin")
        .unwrap_or_else(|| "http://127.0.0.1:7401".into());
    let settings = Settings {
        plex_url,
        server_id,
        kind,
        arr_url,
        origin,
        libraries: options.libraries.clone(),
    };
    let source = settings.render(&runtime)?;
    Config::parse(&source).map_err(
        |_| "init: invalid configuration; check URLs, identifiers, library keys and origin",
    )?;
    let local_config = Config::parse(&settings.render(&output)?)
        .map_err(|_| "init: local configuration invalid")?;
    let plex_token = match plex_token {
        Some(token) => token,
        None => credential(
            &mut options,
            prompt,
            "--plex-token",
            "Plex owner token (hidden)",
        )?,
    };
    let arr_key = credential(
        &mut options,
        prompt,
        "--arr-api-key",
        "Library manager API key (hidden)",
    )?;
    let control = random_secret()?;
    let webhook = random_secret()?;

    // create_dir refuses existing directories and symlinks. Every file uses
    // create_new; no earlier installation or credential is overwritten.
    directory(&output)?;
    directory(&output.join("secrets"))?;
    directory(&output.join("state"))?;
    for (name, content) in [
        ("plex-token", plex_token),
        ("arr-api-key", arr_key),
        ("control", control),
        ("arr-webhook", webhook),
    ] {
        private_file(&output.join("secrets").join(name), &content)?;
    }
    // Publish the config last. A failure may leave a private incomplete directory;
    // deliberately keep it for inspection instead of recursively deleting paths.
    private_file(&output.join("pitcrew.toml"), &source)?;
    Ok(Installation {
        local_config,
        connectivity: options.connectivity,
    })
}

fn required(
    options: &mut Options,
    prompt: &mut impl Prompt,
    flag: &str,
    label: &str,
) -> Result<String> {
    if let Some(value) = options.values.remove(flag) {
        return Ok(value);
    }
    if options.non_interactive {
        return Err("init: missing required input; see init --help");
    }
    prompt.read(label, false)
}

fn credential(
    options: &mut Options,
    prompt: &mut impl Prompt,
    prefix: &str,
    label: &str,
) -> Result<String> {
    let value = if let Some(path) = options.values.remove(&format!("{prefix}-file")) {
        fs::read_to_string(path)
            .map_err(|_| "init: credential file absent, unreadable, or not UTF-8")?
    } else if let Some(name) = options.values.remove(&format!("{prefix}-env")) {
        std::env::var(name)
            .map_err(|_| "init: credential environment variable absent or not UTF-8")?
    } else if options.non_interactive {
        return Err("init: missing credential source; use a secret file or environment variable");
    } else {
        prompt.read(label, true)?
    };
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.len() > 4096 || !value.bytes().all(|b| (0x21..=0x7e).contains(&b))
    {
        return Err("init: credential must contain 1..4096 visible ASCII bytes without spaces");
    }
    Ok(value.to_owned())
}

/// "1 Movies [movie], 2 TV Shows [show]". Titles come from the server, so control
/// characters are dropped and length is capped before they reach the terminal.
fn section_list(server: &Server) -> String {
    if server.sections.is_empty() {
        return "the server reported no libraries".into();
    }
    server
        .sections
        .iter()
        .map(|section| {
            format!(
                "{} {} [{}]",
                printable(&section.key),
                printable(&section.title),
                printable(&section.kind)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn printable(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).take(40).collect()
}

fn random_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| "init: operating system randomness unavailable")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path).map_err(
        |_| "init: cannot create private directory; parent must exist and output must be new",
    )
}

fn private_file(path: &Path, source: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "init: cannot create private file")?;
    file.write_all(source.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|_| "init: private file write failed")
}

struct Settings {
    plex_url: String,
    server_id: String,
    kind: String,
    arr_url: String,
    origin: String,
    libraries: Vec<String>,
}

fn quoted(value: &str) -> String {
    toml::Value::String(value.into()).to_string()
}

fn path(root: &Path, suffix: &str) -> Result<String> {
    root.join(suffix)
        .to_str()
        .map(quoted)
        .ok_or("init: output and runtime paths must be UTF-8")
}

impl Settings {
    fn render(&self, root: &Path) -> Result<String> {
        let libraries = toml::Value::Array(
            self.libraries
                .iter()
                .cloned()
                .map(toml::Value::String)
                .collect(),
        );
        Ok(format!(
            r#"# Generated by pitcrew init. Keep this directory private and outside Git.
# Shadow mode records proposed actions. Live repairs require separate certification.
version = 1
database = {database}

[engine]
mode = "shadow"
listen = "127.0.0.1:7400"
control_secret = {{ file = {control} }}
# Arr fallback reads are opt-in. This never authorizes repair writes.
poll_enabled = false

[stack]
enabled = true
intake_database = {intake}
catalog_snapshot_file = {catalog}
web_listen = "127.0.0.1:7401"
web_public_origin = {origin}

[plex]
url = {plex_url}
server_id = {server_id}
owner_token = {{ file = {plex_token} }}

[[arr]]
id = "primary"
kind = {kind}
url = {arr_url}
api_key = {{ file = {arr_key} }}
webhook_secret = {{ file = {webhook} }}
# Explicit Plex section keys; never inferred from a viewer request.
libraries = {libraries}

[providers]
cloud_opt_in = false
decider = "rules"
reply_writer = "templates"

# Optional extras: add more [[arr]] instances, channels, notifiers or an agent.
# Copy the appropriate sections from config.example.toml; keep credentials in
# private files. The media agent enables byte-level checks; without one,
# missing evidence holds repairs for review. See docs/README.md.
# Do not enable live mode until docs/live-certification.md is complete.
"#,
            database = path(root, "state/pitcrew.sqlite")?,
            control = path(root, "secrets/control")?,
            intake = path(root, "state/intake.sqlite")?,
            catalog = path(root, "state/catalog.json")?,
            plex_token = path(root, "secrets/plex-token")?,
            arr_key = path(root, "secrets/arr-api-key")?,
            webhook = path(root, "secrets/arr-webhook")?,
            origin = quoted(&self.origin),
            plex_url = quoted(&self.plex_url),
            server_id = quoted(&self.server_id),
            kind = quoted(&self.kind),
            arr_url = quoted(&self.arr_url)
        ))
    }
}
