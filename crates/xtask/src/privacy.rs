//! Public-tree hygiene. Diagnostics identify locations, never matched values.
use regex::Regex;
use std::{
    collections::BTreeSet,
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    process::Command,
};

const DENYLIST: &str = ".privacy-denylist";

struct Scanner {
    ips: Regex,
    emails: Regex,
    tokens: Regex,
    credentials: Regex,
    denylist: Vec<String>,
}

impl Scanner {
    fn new(denylist: Vec<String>) -> Self {
        Self {
            ips: Regex::new(r"(?:[0-9]{1,3}\.){3}[0-9]{1,3}|(?:[0-9a-fA-F]{0,4}:){2,}[0-9a-fA-F:.]*").unwrap(),
            emails: Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+").unwrap(),
            tokens: Regex::new(concat!(
                r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|",
                r"sk-(?:proj-|ant-)?[A-Za-z0-9_-]{16,}|xox[baprs]-[A-Za-z0-9-]{16,}|",
                r"AKIA[A-Z0-9]{16}|eyJ[A-Za-z0-9_-]{12,}\.[A-Za-z0-9_-]{12,}\.[A-Za-z0-9_-]{12,})\b|",
                r"(?i:Bearer)\s+[A-Za-z0-9._~+/-]{20,}"
            )).unwrap(),
            credentials: Regex::new(r#"(?i)\b(?:[a-z][a-z0-9]*[_-])*(?:api[_-]?key|access[_-]?token|refresh[_-]?token|auth[_-]?token|password|secret|client[_-]?secret|plex[_-]?token)["']?\s*[:=]\s*(?:"([^"\r\n]*)"|'([^'\r\n]*)'|([^\s,;#"']+))"#).unwrap(),
            denylist,
        }
    }

    fn scan(&self, text: &str) -> Vec<(usize, &'static str)> {
        let mut findings = Vec::new();
        // Split on LF instead of `lines`: embedded CR must not hide a value.
        for (index, line) in text.split('\n').enumerate() {
            let mut add = |rule| findings.push((index + 1, rule));
            if self.ips.find_iter(line).any(|value| {
                let literal = value.as_str().trim_end_matches('.');
                let adjacent = |character: char| {
                    character.is_ascii_alphanumeric()
                        || matches!(character, '_' | '.')
                        || (literal.contains(':') && character == ':')
                };
                let bounded = !line[..value.start()]
                    .chars()
                    .next_back()
                    .is_some_and(adjacent)
                    && !line[value.start() + literal.len()..]
                        .trim_start_matches('.')
                        .chars()
                        .next()
                        .is_some_and(adjacent);
                bounded && literal.parse::<IpAddr>().is_ok_and(|ip| !safe_ip(ip))
            }) {
                add("ip-address");
            }
            if line.contains(&["/", "home", "/"].concat()) {
                add("home-path");
            }
            if self.emails.find_iter(line).any(|value| {
                let email = value.as_str().to_ascii_lowercase();
                let (local, domain) = email.split_once('@').unwrap();
                !matches!(local, "noreply" | "no-reply") && domain != "users.noreply.github.com"
            }) {
                add("email");
            }
            if self.tokens.is_match(line)
                || self.credentials.captures_iter(line).any(|capture| {
                    let matched_value = capture
                        .get(1)
                        .or_else(|| capture.get(2))
                        .or_else(|| capture.get(3))
                        .unwrap();
                    let value = matched_value.as_str();
                    // Plain YAML scalars need space after the colon; a systemd
                    // credential path such as name:/path is not an assignment.
                    let assignment =
                        capture.get(3).is_none() || !line[..matched_value.start()].ends_with(':');
                    let unquoted_literal = capture.get(3).is_none()
                        || value.chars().all(|character| {
                            character.is_ascii_alphanumeric() || "._~+/=-".contains(character)
                        });
                    value.len() >= 20
                        && assignment
                        && unquoted_literal
                        && !credential_reference(value)
                })
            {
                add("token");
            }
            if self.denylist.iter().any(|pattern| line.contains(pattern)) {
                add("local-denylist");
            }
        }
        findings
    }
}

fn credential_reference(value: &str) -> bool {
    ["env:", "file:", "replace-", "example-", "<"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
        || (value.starts_with("${") && value.ends_with('}'))
        || (value.starts_with("$(") && value.ends_with(')'))
        || (value.starts_with("{{") && value.ends_with("}}"))
        || value.strip_prefix('$').is_some_and(|name| {
            name.starts_with(|character: char| character.is_ascii_alphabetic() || character == '_')
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
}

fn safe_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_unspecified() || ip.is_documentation(),
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.segments()[..2] == [0x2001, 0x0db8]
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|ip| safe_ip(IpAddr::V4(ip)))
        }
    }
}

fn format_findings(path: &Path, findings: &[(usize, &'static str)]) -> String {
    findings
        .iter()
        .map(|(line, rule)| format!("{path:?}:{line}: {rule}\n"))
        .collect()
}

pub fn check(root: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .current_dir(root)
        .output()
        .map_err(|_| "privacy: could not list repository files".to_owned())?;
    if !output.status.success() {
        return Err("privacy: git could not list repository files".into());
    }
    let names = String::from_utf8(output.stdout)
        .map_err(|_| "privacy: repository paths must be UTF-8".to_owned())?;
    let paths = names
        .split('\0')
        .filter(|name| !name.is_empty())
        .collect::<BTreeSet<_>>();
    if paths.contains(DENYLIST) {
        return Err("privacy: the local denylist must be git-ignored and untracked".into());
    }
    let denylist = match fs::symlink_metadata(root.join(DENYLIST)) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err("privacy: the local denylist must be a regular file".into());
            }
            fs::read_to_string(root.join(DENYLIST))
                .map_err(|_| "privacy: could not read the local denylist".to_owned())?
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_owned)
                .collect()
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => return Err("privacy: could not inspect the local denylist".into()),
    };
    let scanner = Scanner::new(denylist);
    let mut diagnostics = String::new();
    for name in &paths {
        let path = PathBuf::from(name);
        let absolute = root.join(&path);
        let metadata = match fs::symlink_metadata(&absolute) {
            Ok(metadata) => metadata,
            // Deletions already staged or awaiting staging are not published.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(format!("privacy: could not inspect {path:?}")),
        };
        let bytes = if metadata.file_type().is_symlink() {
            // Inspect the link text. Never follow a link outside the repository.
            fs::read_link(&absolute).map(|target| target.as_os_str().as_encoded_bytes().to_vec())
        } else if metadata.is_file() {
            fs::read(&absolute)
        } else {
            return Err(format!("privacy: unsupported repository entry {path:?}"));
        }
        .map_err(|_| format!("privacy: could not read {path:?}"))?;
        // Binary files are scanned as well, without printing their contents.
        diagnostics.push_str(&format_findings(
            &path,
            &scanner.scan(&String::from_utf8_lossy(&bytes)),
        ));
    }
    if diagnostics.is_empty() {
        eprintln!("privacy: checked {} public-tree files", paths.len());
        Ok(())
    } else {
        Err(format!(
            "privacy check failed (values withheld):\n{diagnostics}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/privacy-tests")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).unwrap();
            let repo = Self(path);
            repo.git(&["init", "--quiet"]);
            repo
        }

        fn git(&self, args: &[&str]) {
            let output = Command::new("git")
                .args(args)
                .current_dir(&self.0)
                .output()
                .unwrap();
            assert!(output.status.success());
        }

        fn write(&self, name: &str, text: &str) {
            fs::write(self.0.join(name), text).unwrap();
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn rejects_private_literals_without_echoing_them() {
        let secret = ["ghp_", &"x".repeat(36)].concat();
        let private_ip = ["100", "90", "80", "70"].join(".");
        let email = ["viewer", "personal.invalid"].join("@");
        let home = ["/", "home", "/", "viewer", "/config"].concat();
        let input = format!("{secret}\n{private_ip}\n{email}\n{home}");
        let scanner = Scanner::new(Vec::new());
        let findings = scanner.scan(&input);
        assert_eq!(
            findings,
            vec![
                (1, "token"),
                (2, "ip-address"),
                (3, "email"),
                (4, "home-path")
            ]
        );
        let diagnostic = format_findings(Path::new("fixture.txt"), &findings);
        for value in [secret, private_ip, email, home] {
            assert!(!diagnostic.contains(&value));
        }
    }

    #[test]
    fn documentation_and_bind_addresses_and_noreply_are_allowed() {
        let input = "127.0.0.1 0.0.0.0 192.0.2.1 198.51.100.20 203.0.113.4 \
                     [::1] [::] [2001:db8::1] \
                     contributor@users.noreply.github.com noreply@example.invalid";
        assert!(Scanner::new(Vec::new()).scan(input).is_empty());
    }

    #[test]
    fn rust_paths_are_not_ipv6_literals() {
        assert!(
            Scanner::new(Vec::new())
                .scan(
                    "use std::net::IpAddr; crate::config::load(); IpAddr::V4(ip); Option::Some(1)"
                )
                .is_empty()
        );
    }

    #[test]
    fn detects_ipv4_with_port_in_url() {
        let address = ["10", "20", "30", "40"].join(".");
        assert_eq!(
            Scanner::new(Vec::new()).scan(&format!("http://{address}:9000")),
            vec![(1, "ip-address")]
        );
    }

    #[test]
    fn private_ip_addresses_before_sentence_punctuation_are_rejected() {
        let scanner = Scanner::new(Vec::new());
        for address in [
            ["10", "20", "30", "40"].join("."),
            ["fd00", "", "1"].join(":"),
        ] {
            for punctuation in [".", "...", ",", ";", ")", "!"] {
                assert_eq!(
                    scanner.scan(&format!("The host is {address}{punctuation}")),
                    vec![(1, "ip-address")],
                    "sentence punctuation must not hide an address"
                );
            }
        }
    }

    #[test]
    fn email_boundaries_do_not_include_url_or_config_syntax() {
        assert!(
            Scanner::new(Vec::new())
                .scan("https://noreply@example.invalid/\nfrom_address='noreply@example.invalid'")
                .is_empty()
        );
    }

    #[test]
    fn checks_ipv6_private_and_public_and_rejects_mapped_bypass() {
        let scanner = Scanner::new(Vec::new());
        for address in [
            ["fd00", "12", "", "5"].join(":"),
            ["2001", "4860", "4860", "", "8888"].join(":"),
            ["", "", "ffff", &["10", "20", "30", "40"].join(".")].join(":"),
        ] {
            assert!(
                scanner
                    .scan(&format!("https://[{address}]:9000"))
                    .iter()
                    .any(|(_, rule)| *rule == "ip-address")
            );
        }
    }

    #[test]
    fn recognizes_credentials_but_not_secret_references_or_placeholders() {
        let scanner = Scanner::new(Vec::new());
        let value = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4";
        for prefix in ["api_key = ", "\"access_token\": ", "password: "] {
            assert_eq!(
                scanner.scan(&format!("{prefix}\"{value}\"")),
                vec![(1, "token")]
            );
        }
        assert!(scanner.scan("api_key = \"env:ARR_API_KEY\"\nsecret = \"file:/run/secrets/agent\"\napi_key = \"replace-with-your-api-key\"").is_empty());
        let bearer = format!("Authorization: Bearer {value}");
        assert_eq!(scanner.scan(&bearer), vec![(1, "token")]);
    }

    #[test]
    fn service_prefixed_and_unquoted_credentials_are_rejected() {
        let scanner = Scanner::new(Vec::new());
        let value = ["synthetic", &"a1".repeat(16)].concat();
        for key in [
            "RADARR_API_KEY",
            "service_access_token",
            "API_KEY",
            "password",
        ] {
            for assignment in [
                format!("{key}={value}"),
                format!("export {key}='{value}'"),
                format!("{key}: {value} # example"),
                format!("\"{key}\": \"{value}\""),
            ] {
                assert_eq!(scanner.scan(&assignment), vec![(1, "token")]);
            }
        }
    }

    #[test]
    fn credential_assignments_allow_shell_config_and_template_references() {
        let scanner = Scanner::new(Vec::new());
        for value in [
            "$PITCREW_SYNTHETIC_API_KEY",
            "${PITCREW_SYNTHETIC_API_KEY}",
            "${PITCREW_SYNTHETIC_API_KEY:-replace-this-value}",
            "$(read-synthetic-secret-file)",
            "${{ secrets.PITCREW_SYNTHETIC_API_KEY }}",
            "{{ vault.PITCREW_SYNTHETIC_API_KEY }}",
            "env:PITCREW_SYNTHETIC_API_KEY",
            "file:/run/secrets/synthetic-key",
            "replace-with-your-api-key",
            "example-synthetic-api-key",
            "<your-synthetic-api-key>",
        ] {
            for assignment in [
                format!("RADARR_API_KEY={value}"),
                format!("RADARR_API_KEY='{value}'"),
                format!("\"service_api_key\": \"{value}\""),
            ] {
                assert!(scanner.scan(&assignment).is_empty());
            }
        }
        assert!(
            scanner
                .scan("api_key = { env = \"PITCREW_SYNTHETIC_API_KEY\" }")
                .is_empty()
        );
        assert!(
            scanner
                .scan("api_key = { file = \"/run/secrets/synthetic-key\" }")
                .is_empty()
        );
        assert!(
            scanner
                .scan("# LoadCredential=agent-secret:/etc/pitcrew/agent-secret")
                .is_empty()
        );
    }

    #[test]
    fn denylist_is_literal_and_deduplicates_per_line() {
        let scanner = Scanner::new(vec!["private.example".into(), "private".into()]);
        assert_eq!(
            scanner.scan("safe\nprivate.example private.example"),
            vec![(2, "local-denylist")]
        );
        assert!(
            scanner
                .scan("privateXexample")
                .iter()
                .all(|(_, rule)| *rule == "local-denylist")
        );
        assert!(
            Scanner::new(vec!["private.example".into()])
                .scan("privateXexample")
                .is_empty()
        );
    }

    #[test]
    fn scans_public_tree_and_ignores_private_local_files() {
        let repo = Repo::new();
        let value = ["private", ".example"].concat();
        repo.write(".gitignore", ".privacy-denylist\nprivate.txt\n");
        repo.write(DENYLIST, &format!("# local literal patterns\n\n{value}\n"));
        repo.write("private.txt", &value);
        repo.write("tracked.txt", "synthetic");
        repo.git(&["add", "tracked.txt"]);
        assert!(check(&repo.0).is_ok());
        repo.write("new.txt", &value);
        let error = check(&repo.0).unwrap_err();
        assert!(error.contains("new.txt"));
        assert!(error.contains("local-denylist"));
        assert!(!error.contains(&value));
        fs::remove_file(repo.0.join("new.txt")).unwrap();
        // An ignore rule must never hide an already tracked file.
        repo.git(&["add", "--force", "private.txt"]);
        assert!(check(&repo.0).unwrap_err().contains("private.txt"));
    }

    #[test]
    fn refuses_a_public_or_forced_tracked_denylist() {
        let repo = Repo::new();
        repo.write(DENYLIST, "synthetic-private-host");
        assert!(
            check(&repo.0)
                .unwrap_err()
                .contains("git-ignored and untracked")
        );
        repo.write(".gitignore", ".privacy-denylist\n");
        assert!(check(&repo.0).is_ok());
        repo.git(&["add", "--force", DENYLIST]);
        assert!(
            check(&repo.0)
                .unwrap_err()
                .contains("git-ignored and untracked")
        );
    }

    #[test]
    fn binary_data_is_checked_and_deleted_files_are_accepted() {
        let repo = Repo::new();
        let email = ["viewer", "personal.invalid"].join("@");
        let mut bytes = vec![0xff, 0, 0x80];
        bytes.extend_from_slice(email.as_bytes());
        fs::write(repo.0.join("capture.bin"), bytes).unwrap();
        assert!(check(&repo.0).unwrap_err().contains(":1: email"));
        repo.git(&["add", "capture.bin"]);
        fs::remove_file(repo.0.join("capture.bin")).unwrap();
        assert!(check(&repo.0).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn inspects_symlink_text_without_following_the_target() {
        let repo = Repo::new();
        std::os::unix::fs::symlink("missing-public-target", repo.0.join("link")).unwrap();
        assert!(check(&repo.0).is_ok());
        fs::remove_file(repo.0.join("link")).unwrap();
        let target = ["/", "home", "/", "viewer"].concat();
        std::os::unix::fs::symlink(target, repo.0.join("link")).unwrap();
        assert!(check(&repo.0).unwrap_err().contains("home-path"));
    }
}
