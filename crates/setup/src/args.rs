use crate::Result;
use std::collections::BTreeMap;

pub const USAGE: &str = "pitcrew init [OPTIONS]
Creates a new private installation and runs doctor (offline by default).

  --output DIR                  New directory (default: ./runtime)
  --runtime-root ABS            Paths inside the runtime (default: output directory)
  --non-interactive             Require every minimum input; never prompt
  --plex-url URL                Plex server HTTP(S) root URL (no path prefix)
  --plex-server-id ID           Plex machineIdentifier from the server's identity
  --discover                    Read the server identifier and library list from Plex
                                (two bounded authenticated GETs; nothing is written)
  --plex-token-file FILE        Read the owner token from a file
  --plex-token-env NAME         Read the owner token from this environment variable
  --arr-kind sonarr|radarr      One initial library manager
  --arr-url URL                 Manager HTTP(S) base URL
  --arr-api-key-file FILE       Read its API key from a file
  --arr-api-key-env NAME        Read its API key from this environment variable
  --library KEY                 Plex library section key (repeat for each mapping)
  --public-origin ORIGIN        Viewer origin (default: http://127.0.0.1:7401)
  --connectivity                Explicitly enable bounded read-only doctor requests
  --help                        Show this help

Interactive mode prompts for missing minimum inputs and hides typed secrets.
Token/API key values are not accepted as command-line arguments.
No output directory is overwritten. Optional integrations can be added to the TOML.";

#[derive(Default)]
pub struct Options {
    pub(crate) values: BTreeMap<String, String>,
    pub(crate) libraries: Vec<String>,
    pub(crate) non_interactive: bool,
    pub(crate) connectivity: bool,
    pub(crate) discover: bool,
}

impl Options {
    /// None means help. Errors deliberately exclude argument values.
    pub fn parse(args: &[String]) -> Result<Option<Self>> {
        if args == ["--help"] {
            return Ok(None);
        }
        let mut options = Self::default();
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--non-interactive" if !options.non_interactive => options.non_interactive = true,
                "--connectivity" if !options.connectivity => options.connectivity = true,
                "--discover" if !options.discover => options.discover = true,
                "--library" => options.libraries.push(value(&mut args)?),
                "--output" | "--runtime-root" | "--plex-url" | "--plex-server-id"
                | "--plex-token-file" | "--plex-token-env" | "--arr-kind" | "--arr-url"
                | "--arr-api-key-file" | "--arr-api-key-env" | "--public-origin" => {
                    if options
                        .values
                        .insert(flag.clone(), value(&mut args)?)
                        .is_some()
                    {
                        return Err("init: option supplied more than once; see init --help");
                    }
                }
                _ => return Err("init: unrecognized or repeated option; see init --help"),
            }
        }
        for prefix in ["--plex-token", "--arr-api-key"] {
            if options.values.contains_key(&format!("{prefix}-file"))
                && options.values.contains_key(&format!("{prefix}-env"))
            {
                return Err("init: select one credential source: file or environment variable");
            }
        }
        Ok(Some(options))
    }
}

fn value(args: &mut std::slice::Iter<'_, String>) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty() && !value.starts_with("--"))
        .cloned()
        .ok_or("init: option requires a value; see init --help")
}
