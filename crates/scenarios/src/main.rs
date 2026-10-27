use std::{
    env,
    path::Path,
    process::{self, Command},
};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let args: Vec<_> = env::args().skip(1).collect();
    let outcome = match args.as_slice() {
        [command] if command == "check" => pitcrew_scenarios::check_suite(&root.join("scenarios")).map(|message| println!("{message}")),
        [command, directory] if command == "check" => pitcrew_scenarios::check_suite(Path::new(directory)).map(|message| println!("{message}")),
        [command, rest @ ..] if command == "external" || command == "python" => {
            match rest.windows(2).position(|pair| pair[0] == "--adapter") {
                None if rest.is_empty() => { println!("Optional external replay skipped: supply --adapter PATH for a reviewed local adapter."); Ok(()) },
                None => Err("external replay requires --adapter PATH when options are supplied".into()),
                Some(index) => {
                    let adapter = &rest[index + 1];
                    if adapter.starts_with('-') || !Path::new(adapter).is_file() {
                        Err("external replay requires --adapter PATH naming an existing regular file".into())
                    } else {
                        let forwarded: Vec<_> = rest[..index].iter().chain(rest[index+2..].iter()).collect();
                        let interpreter = rest.windows(2).find(|pair| pair[0] == "--python").map_or("python3", |pair| pair[1].as_str());
                        match Command::new(interpreter).arg("-B").arg(adapter).args(forwarded).status() {
                            Ok(status) if status.success() => Ok(()),
                            Ok(status) => Err(format!("external scenario adapter exited {status}")),
                            Err(error) => Err(error.to_string()),
                        }
                    }
                }
            }
        },
        _ => Err("usage: cargo run -p pitcrew-scenarios -- check [DIRECTORY] | external --adapter PATH [ADAPTER_OPTIONS]".into()),
    };
    if let Err(error) = outcome {
        eprintln!("{error}");
        process::exit(1);
    }
}
