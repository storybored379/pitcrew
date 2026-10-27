//! Offline installation setup. Credentials are never accepted as literal CLI values.
//! The caller runs its existing doctor against the returned local configuration.

mod args;
mod terminal;
mod write;

pub use args::{Options, USAGE};
pub use terminal::Terminal;

pub type Result<T> = std::result::Result<T, &'static str>;

/// Interactive input is injected so tests never need a real terminal or network.
pub trait Prompt {
    fn read(&mut self, label: &str, secret: bool) -> Result<String>;
}

/// Reads the server identifier and library list for `--discover`. Injected so tests
/// never need a network; the binary implements it with bounded Plex reads.
pub trait Discover {
    fn discover(&mut self, plex_url: &str, token: &str) -> Result<Server>;
}

pub struct Server {
    pub id: String,
    pub sections: Vec<Section>,
}

pub struct Section {
    pub key: String,
    pub kind: String,
    pub title: String,
}

pub struct Installation {
    /// References local secret files even when the generated TOML targets a container.
    pub local_config: pitcrew_config::Config,
    pub connectivity: bool,
}

/// Collect inputs, validate them, then create a fresh private installation.
pub fn create(options: Options, prompt: &mut impl Prompt) -> Result<Installation> {
    write::create(options, prompt, None)
}

/// As [`create`], with a discovery client used only when `--discover` is given.
pub fn create_with(
    options: Options,
    prompt: &mut impl Prompt,
    discover: &mut dyn Discover,
) -> Result<Installation> {
    write::create(options, prompt, Some(discover))
}
