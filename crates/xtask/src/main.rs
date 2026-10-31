//! Repository checks. A failed or missing check is always a nonzero exit.
mod privacy;
use std::{
    path::Path,
    process::{Command, ExitCode},
};

fn run(root: &Path, args: &[&str]) -> Result<(), String> {
    eprintln!("+ cargo {}", args.join(" "));
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|error| format!("could not start cargo: {error}"))?;
    if !status.success() {
        return Err(format!("cargo {} failed ({status})", args.join(" ")));
    }
    Ok(())
}

fn check(root: &Path) -> Result<(), String> {
    privacy::check(root)?;
    run(root, &["fmt", "--all", "--", "--check"])?;
    run(
        root,
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run(root, &["test", "--workspace", "--locked"])?;
    run(
        root,
        &["run", "--locked", "-p", "pitcrew-scenarios", "--", "check"],
    )?;
    eprintln!("+ python3 -B -m unittest discover -s crates/scenarios/python");
    let status = Command::new("python3")
        .args([
            "-B",
            "-m",
            "unittest",
            "discover",
            "-s",
            "crates/scenarios/python",
        ])
        .current_dir(root)
        .status()
        .map_err(|error| format!("could not start Python adapter checks: {error}"))?;
    if !status.success() {
        return Err(format!("Python adapter checks failed ({status})"));
    }
    Ok(())
}

const MUTATION_PACKAGES: &[&str] = &[
    "pitcrew-agent-core",
    "pitcrew-arbiter",
    "pitcrew-engine",
    "pitcrew-store",
    "pitcrew-arr",
    "pitcrew",
    "pitcrew-diagnosis",
    "pitcrew-deciders",
    "pitcrew-intake",
    "pitcrew-intake-store",
    "pitcrew-web",
    "pitcrew-bot",
    "pitcrew-drill",
    "pitcrew-sync",
    "pitcrew-bridge",
    "pitcrew-admin",
    "pitcrew-setup",
    "pitcrew-notify",
];

fn mutations(root: &Path, selected: Option<&str>) -> Result<(), String> {
    if selected.is_some_and(|selected| {
        selected != "pitcrew-domain" && !MUTATION_PACKAGES.contains(&selected)
    }) {
        return Err("unknown mutation package; use a package listed in the CI gate".into());
    }
    // Never mutate the working tree: cargo-mutants uses disposable copies.
    // No exclusions: every generated mutant in the lifecycle, adapters,
    // control plane, diagnosis, deciders and intake surfaces must be caught or
    // unviable.
    if selected.is_none_or(|selected| selected == "pitcrew-domain") {
        run(
            root,
            &[
                "mutants",
                "--package",
                "pitcrew-domain",
                "--test-package",
                "pitcrew-domain",
                "--test-package",
                "pitcrew-scenarios",
                "--jobs",
                "2",
                "--timeout",
                "120",
                "--build-timeout",
                "900",
                "--copy-target=true",
                "--cargo-arg=--locked",
                "--cargo-arg=--offline",
            ],
        )?;
    }
    for &package in MUTATION_PACKAGES {
        if selected.is_some_and(|selected| selected != package) {
            continue;
        }
        let mut args = vec![
            "mutants",
            "--package",
            package,
            "--test-package",
            package,
            "--jobs",
            "2",
            "--timeout",
            "120",
            "--build-timeout",
            "900",
            "--copy-target=true",
            "--cargo-arg=--locked",
            "--cargo-arg=--offline",
        ];
        // New adapters have dedicated contract/fault suites. Mutate them with
        // those suites instead of rerunning unrelated exhaustive crash tests
        // for each predicate. The remainder still uses the entire package's
        // suite, including any future source files: these groups omit no code.
        // The full workspace suite also runs before any mutation checks.
        type Group = (&'static str, &'static [&'static str], Option<&'static str>);
        let groups: &[Group] = match package {
            "pitcrew-engine" => &[
                (
                    "crates/engine/src/live.rs",
                    &["--cargo-arg=--test=live"],
                    None,
                ),
                (
                    "crates/engine/src/target.rs",
                    &["--cargo-arg=--test=target"],
                    None,
                ),
                (
                    "crates/engine/src/delivery.rs",
                    &["--cargo-arg=--test=delivery"],
                    None,
                ),
                (
                    "crates/engine/src/observer.rs",
                    &["--cargo-arg=--test=observer"],
                    None,
                ),
                (
                    "crates/engine/src/webhook.rs",
                    &["--cargo-arg=--test=intake"],
                    None,
                ),
            ],
            "pitcrew-store" => &[
                (
                    "crates/store/src/policy.rs",
                    &["--cargo-arg=--lib"],
                    Some("policy::tests::"),
                ),
                (
                    "crates/store/src/delivery.rs",
                    &["--cargo-arg=--test=delivery"],
                    None,
                ),
            ],
            "pitcrew" => &[
                (
                    "bins/pitcrew/src/python.rs",
                    &["--cargo-arg=--lib", "--cargo-arg=--test=python"],
                    None,
                ),
                (
                    "bins/pitcrew/src/admin.rs",
                    &["--cargo-arg=--test=admin"],
                    None,
                ),
            ],
            _ => &[],
        };
        for (source, tests, filter) in groups {
            let name = Path::new(source).file_stem().unwrap().to_str().unwrap();
            let output = format!("mutants.out-{package}-{name}");
            let mut grouped = args.clone();
            grouped.extend(["--file", source, "--output", &output]);
            grouped.extend_from_slice(tests);
            if let Some(filter) = filter {
                grouped.extend(["--", filter]);
            }
            run(root, &grouped)?;
        }
        for (source, _, _) in groups {
            args.extend(["--exclude", source]);
        }
        let output = format!("mutants.out-{package}");
        args.extend(["--output", &output]);
        run(root, &args)?;
    }
    Ok(())
}

fn ci(root: &Path) -> Result<(), String> {
    check(root)?;
    mutations(root, None)
}

fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match args.as_slice() {
        [command] if command == "ci" => ci(root),
        [command] if command == "check" => check(root),
        [command] if command == "mutants" => mutations(root, None),
        [command, package] if command == "mutants" => mutations(root, Some(package)),
        [command] if command == "privacy" => privacy::check(root),
        _ => Err("usage: cargo xtask <check|privacy|ci> or cargo xtask mutants [package]\nci and mutants require cargo-mutants 27.1.0 (cargo install cargo-mutants --version 27.1.0 --locked)".to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_matrix_covers_the_complete_mutation_gate() {
        let workflow = include_str!("../../../.github/workflows/ci.yml");
        let mut scheduled = workflow
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("- ")
                    .filter(|package| package.starts_with("pitcrew"))
            })
            .collect::<Vec<_>>();
        let mut required = MUTATION_PACKAGES.to_vec();
        required.push("pitcrew-domain");
        scheduled.sort_unstable();
        required.sort_unstable();
        assert_eq!(scheduled, required);
    }

    #[test]
    fn unknown_mutation_package_fails_before_starting_cargo() {
        assert_eq!(
            mutations(Path::new("does-not-exist"), Some("unknown")).unwrap_err(),
            "unknown mutation package; use a package listed in the CI gate"
        );
    }
}
