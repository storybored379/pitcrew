//! Offline drill drivers. They have no URL or live-server option.
use pitcrew_drill::{Corpus, Observation};
use std::{fs, process::ExitCode};

async fn execute() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let [command, corpus_path, data_path, reference, rest @ ..] = args.as_slice()
        && (command == "external" || command == "python")
        && rest.len() <= 1
    {
        let corpus: Corpus = serde_json::from_str(&fs::read_to_string(corpus_path)?)?;
        let rows = pitcrew_drill::drivers::run_external(
            &corpus,
            reference.as_ref(),
            rest.first()
                .map_or(std::path::Path::new("python3"), |value| value.as_ref()),
        )?;
        save_and_score(&corpus, &rows, data_path)?;
        return Ok(());
    }
    let [command, corpus_path, data_path] = args.as_slice() else {
        return Err("usage: pitcrew-drill run|run-channel|score CORPUS OBSERVATIONS | export-external CORPUS REQUESTS | external CORPUS OBSERVATIONS ADAPTER_DIRECTORY [PYTHON]".into());
    };
    let corpus: Corpus = serde_json::from_str(&fs::read_to_string(corpus_path)?)?;
    match command.as_str() {
        "run" => {
            let rows = pitcrew_drill::run(&corpus)?;
            save_and_score(&corpus, &rows, data_path)?;
        }
        "run-channel" => {
            let rows = pitcrew_drill::drivers::run_test_channel(&corpus).await?;
            save_and_score(&corpus, &rows, data_path)?;
        }
        "score" => {
            let rows: Vec<Observation> = fs::read_to_string(data_path)?
                .lines()
                .map(serde_json::from_str)
                .collect::<Result<_, _>>()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&pitcrew_drill::score(&corpus, &rows)?)?
            );
        }
        "export-external" | "export-python" => {
            let rows = pitcrew_drill::external_requests(&corpus)?;
            let bytes = rows
                .iter()
                .map(serde_json::to_string)
                .collect::<Result<Vec<_>, _>>()?
                .join("\n")
                + "\n";
            use std::io::Write;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(data_path)?
                .write_all(bytes.as_bytes())?;
        }
        _ => return Err("unknown drill command".into()),
    }
    Ok(())
}
fn save_and_score(
    corpus: &Corpus,
    rows: &[Observation],
    data_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = rows
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n")
        + "\n";
    // Refuse to overwrite observations: results are evidence, not a cache.
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(data_path)?
        .write_all(bytes.as_bytes())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&pitcrew_drill::score(corpus, rows)?)?
    );
    Ok(())
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match execute().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("drill: {error}");
            ExitCode::FAILURE
        }
    }
}
