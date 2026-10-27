//! Run the real CLI against temporary files and synthetic loopback upstreams.
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

fn command(root: &Path, url: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pitcrew"));
    command
        .current_dir(root)
        .args([
            "init",
            "--non-interactive",
            "--plex-url",
            url,
            "--plex-server-id",
            "synthetic-server",
            "--plex-token-env",
            "INIT_TEST_PLEX",
            "--arr-kind",
            "radarr",
            "--arr-url",
            url,
            "--arr-api-key-env",
            "INIT_TEST_ARR",
            "--library",
            "2",
        ])
        .env("INIT_TEST_PLEX", "synthetic-plex-credential")
        .env("INIT_TEST_ARR", "synthetic-arr-credential");
    command
}

fn output_text(output: &Output) -> String {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains("synthetic-plex-credential"));
    assert!(!text.contains("synthetic-arr-credential"));
    text
}

#[test]
fn actual_cli_runs_offline_doctor_with_local_paths_for_a_container_install() {
    let temp = tempfile::tempdir().unwrap();
    let output = command(temp.path(), "https://upstream.example.invalid")
        .args(["--runtime-root", "/config"])
        .output()
        .unwrap();
    let text = output_text(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("Doctor passed"));
    let report: serde_json::Value =
        serde_json::from_slice(output.stdout.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    assert_eq!(report["checks"].as_array().unwrap().len(), 6);
    assert_eq!(report["checks"][5]["field"], "connectivity");
    assert_eq!(report["checks"][5]["status"], "skipped");
    let install = temp.path().join("runtime");
    assert!(install.join("pitcrew.toml").exists());
    assert!(!install.join("state/pitcrew.sqlite").exists());
    let saved = fs::read_to_string(install.join("pitcrew.toml")).unwrap();
    assert!(saved.contains("/config/secrets/plex-token"));
    assert!(!saved.contains("synthetic-plex-credential"));
    let repeated = command(temp.path(), "https://upstream.example.invalid")
        .output()
        .unwrap();
    assert!(!repeated.status.success());
    assert!(output_text(&repeated).contains("already exists"));
    assert_eq!(
        fs::read_to_string(install.join("pitcrew.toml")).unwrap(),
        saved
    );
}

#[test]
fn actual_cli_help_and_errors_never_echo_argument_values() {
    let temp = tempfile::tempdir().unwrap();
    let help = Command::new(env!("CARGO_BIN_EXE_pitcrew"))
        .args(["init", "--help"])
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(output_text(&help).contains("--plex-token-env"));
    for args in [
        vec!["init", "--non-interactive"],
        vec!["init", "--plex-token", "synthetic-plex-credential"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_pitcrew"))
            .args(args)
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        output_text(&output);
    }
    assert!(!temp.path().join("runtime").exists());
}

#[test]
fn init_cannot_publish_a_plex_url_rejected_by_the_viewer() {
    for (url, accepted) in [
        ("https://plex.example.invalid/plex", false),
        ("https://plex.example.invalid/", true),
        ("http://plex.example.invalid:32400", true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let output = command(temp.path(), url).output().unwrap();
        let text = output_text(&output);
        let config = temp.path().join("runtime/pitcrew.toml");
        if output.status.success() {
            let check = Command::new(env!("CARGO_BIN_EXE_pitcrew"))
                .args(["web", "--config"])
                .arg(&config)
                .arg("--check")
                .output()
                .unwrap();
            assert!(
                check.status.success(),
                "accepted init URL must pass viewer configuration check: {}",
                output_text(&check)
            );
        }
        assert_eq!(output.status.success(), accepted, "{text}");
        if !accepted {
            assert!(!temp.path().join("runtime").exists());
            assert!(text.contains("Plex URL must use the server root without a path prefix"));
            assert!(!text.contains(url));
        }
    }
}

#[test]
fn explicit_connectivity_uses_read_only_fakes_and_preserves_files_on_failure() {
    for status in [200, 503] {
        let temp = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let fake = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut paths = Vec::new();
            while paths.len() < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut request = Vec::new();
                        while !request.ends_with(b"\r\n\r\n") {
                            let mut byte = [0];
                            stream.read_exact(&mut byte).unwrap();
                            request.push(byte[0]);
                        }
                        let request = String::from_utf8(request).unwrap();
                        let first = request.lines().next().unwrap();
                        assert!(first.starts_with("GET "));
                        paths.push(first.split_whitespace().nth(1).unwrap().to_owned());
                        write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("fake listener failed: {error}"),
                }
            }
            paths
        });
        let output = command(temp.path(), &url)
            .arg("--connectivity")
            .output()
            .unwrap();
        assert_eq!(
            fake.join().unwrap(),
            ["/library/sections", "/api/v3/system/status"]
        );
        let text = output_text(&output);
        assert_eq!(output.status.success(), status == 200, "{text}");
        assert!(temp.path().join("runtime/pitcrew.toml").exists());
        if status == 503 {
            assert!(text.contains("files saved, but doctor checks failed"));
            assert!(!text.contains("Doctor passed"));
        }
    }
}

/// Serves `/identity` and `/library/sections` from loopback, recording each GET path.
fn fake_plex(listener: TcpListener) -> thread::JoinHandle<Vec<String>> {
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut paths = Vec::new();
        while paths.len() < 2 && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        stream.read_exact(&mut byte).unwrap();
                        request.push(byte[0]);
                    }
                    let request = String::from_utf8(request).unwrap();
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("x-plex-token: synthetic-plex-credential\r\n")
                    );
                    let path = request
                        .lines()
                        .next()
                        .unwrap()
                        .strip_prefix("GET ")
                        .unwrap()
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .to_owned();
                    let body = match path.as_str() {
                        "/identity" => {
                            r#"{"MediaContainer":{"machineIdentifier":"synthetic-found"}}"#
                                .to_owned()
                        }
                        // A large server's library list is several KiB; the cap must admit it.
                        "/library/sections" => {
                            let mut sections = vec![
                                r#"{"key":"2","type":"movie","title":"Films"}"#.to_owned(),
                                r#"{"key":"5","type":"show","title":"Shows"}"#.to_owned(),
                            ];
                            sections.extend((100..200).map(|key| {
                                format!(r#"{{"key":"{key}","type":"artist","title":"Synthetic Collection {key}"}}"#)
                            }));
                            let body = format!(
                                r#"{{"MediaContainer":{{"size":{},"Directory":[{}]}}}}"#,
                                sections.len(),
                                sections.join(",")
                            );
                            assert!(body.len() > 4096);
                            body
                        }
                        _ => panic!("unexpected discovery path"),
                    };
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                    paths.push(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("fake listener failed: {error}"),
            }
        }
        paths
    })
}

/// `command` without `--plex-server-id`, so discovery must supply it.
fn discover_command(root: &Path, url: &str, library: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pitcrew"));
    command
        .current_dir(root)
        .args([
            "init",
            "--non-interactive",
            "--discover",
            "--plex-url",
            url,
            "--plex-token-env",
            "INIT_TEST_PLEX",
            "--arr-kind",
            "radarr",
            "--arr-url",
            "http://radarr.example.invalid:7878",
            "--arr-api-key-env",
            "INIT_TEST_ARR",
            "--library",
            library,
        ])
        .env("INIT_TEST_PLEX", "synthetic-plex-credential")
        .env("INIT_TEST_ARR", "synthetic-arr-credential");
    command
}

#[test]
fn discover_reads_identity_and_sections_from_plex_only() {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let fake = fake_plex(listener);
    let output = discover_command(temp.path(), &url, "2").output().unwrap();
    assert_eq!(fake.join().unwrap(), ["/identity", "/library/sections"]);
    let text = output_text(&output);
    assert!(output.status.success(), "{text}");
    let source = fs::read_to_string(temp.path().join("runtime/pitcrew.toml")).unwrap();
    assert!(source.contains("\"synthetic-found\""));
}

#[test]
fn discover_refuses_a_library_the_server_does_not_have() {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let fake = fake_plex(listener);
    let output = discover_command(temp.path(), &url, "8").output().unwrap();
    fake.join().unwrap();
    let text = output_text(&output);
    assert!(!output.status.success());
    assert!(
        text.contains("library key is not a section on the discovered server"),
        "{text}"
    );
    assert!(!temp.path().join("runtime").exists());
}
