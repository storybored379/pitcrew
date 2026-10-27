use crate::{Error, Result};
pub use pitcrew_config::admin::AdminUi as Config;
use serde::Deserialize;
use std::collections::BTreeSet;
use url::Url;

pub fn parse(source: &str) -> Result<Config> {
    #[derive(Deserialize)]
    struct Document {
        admin: Config,
    }
    let document: Document = toml::from_str(source).map_err(|_| Error::Input)?;
    validate(&document.admin)?;
    Ok(document.admin)
}

pub fn validate(config: &Config) -> Result<()> {
    let origin = endpoint(&config.public_origin)?;
    if origin.origin().ascii_serialization() != config.public_origin
        || config.client_id.trim().is_empty()
        || config.admin_account_ids.is_empty()
        || config
            .admin_account_ids
            .iter()
            .any(|id| id.parse::<u64>().ok().filter(|n| *n != 0).is_none())
        || config
            .admin_account_ids
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != config.admin_account_ids.len()
        || config.database.as_os_str().is_empty()
        || config.engine_database.as_os_str().is_empty()
        || config.intake_database.as_os_str().is_empty()
    {
        return Err(Error::Input);
    }
    endpoint(&config.plex_auth_url)?;
    if let Some(api) = &config.api {
        endpoint(&api.url)?;
        api.secret
            .validate("admin.api.secret")
            .map_err(|_| Error::Input)?;
    }
    Ok(())
}

/// Before creating the writable admin journal, reject aliases of source files
/// and SQLite sidecars. Existing hardlinks and dangling symlinks fail closed.
pub fn protect_sources(config: &Config) -> Result<()> {
    use std::{
        fs,
        path::{Path, PathBuf},
    };
    fn canonical(path: &Path) -> Result<PathBuf> {
        match fs::symlink_metadata(path) {
            Ok(_) => fs::canonicalize(path).map_err(|_| Error::Input),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                Ok(fs::canonicalize(parent)
                    .map_err(|_| Error::Input)?
                    .join(path.file_name().ok_or(Error::Input)?))
            }
            Err(_) => Err(Error::Input),
        }
    }
    let destination = canonical(&config.database)?;
    for destination_suffix in ["", "-wal", "-shm", "-journal"] {
        let mut destination_path = destination.as_os_str().to_owned();
        destination_path.push(destination_suffix);
        let destination_path = canonical(Path::new(&destination_path))?;
        for source in [&config.engine_database, &config.intake_database] {
            let source = fs::canonicalize(source).map_err(|_| Error::Input)?;
            for suffix in ["", "-wal", "-shm", "-journal"] {
                let mut path = source.as_os_str().to_owned();
                path.push(suffix);
                let path = PathBuf::from(path);
                if destination_path == canonical(&path)? {
                    return Err(Error::Input);
                }
                #[cfg(unix)]
                if let (Ok(a), Ok(b)) = (fs::metadata(&destination_path), fs::metadata(&path)) {
                    use std::os::unix::fs::MetadataExt;
                    if a.dev() == b.dev() && a.ino() == b.ino() {
                        return Err(Error::Input);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Credentials only travel over HTTPS, except explicitly configured loopback
/// transports for local ingress and synthetic tests. Redirects are never followed.
pub fn endpoint(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| Error::Input)?;
    if !(url.scheme() == "https"
        || url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost")))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(Error::Input);
    }
    Ok(url)
}
