use crate::{Prompt, Result};
use rustix::termios::{self, LocalModes, OptionalActions, SpecialCodeIndex, Termios};
use std::io::{self, BufRead, IsTerminal, Read, Write};

pub struct Terminal;

struct Restore(Termios);
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &self.0);
    }
}

impl Prompt for Terminal {
    fn read(&mut self, label: &str, secret: bool) -> Result<String> {
        if !io::stdin().is_terminal() {
            return Err(
                "init: interactive input needs a terminal; supply flags and --non-interactive",
            );
        }
        let _restore = if secret {
            let original = termios::tcgetattr(io::stdin())
                .map_err(|_| "init: cannot disable terminal echo; use a secret file instead")?;
            let mut hidden = original.clone();
            // Read cancellation as a byte so the restoration guard runs on Ctrl-C.
            for flag in [
                LocalModes::ECHO,
                LocalModes::ECHONL,
                LocalModes::ICANON,
                LocalModes::ISIG,
            ] {
                hidden.local_modes.remove(flag);
            }
            hidden.special_codes[SpecialCodeIndex::VMIN] = 1;
            hidden.special_codes[SpecialCodeIndex::VTIME] = 0;
            termios::tcsetattr(io::stdin(), OptionalActions::Now, &hidden)
                .map_err(|_| "init: cannot disable terminal echo; use a secret file instead")?;
            Some(Restore(original))
        } else {
            None
        };
        eprint!("{label}: ");
        io::stderr()
            .flush()
            .map_err(|_| "init: prompt output failed")?;
        if secret {
            let result = read_secret(&mut io::stdin().lock());
            eprintln!();
            return result;
        }
        let mut value = String::new();
        let length = io::stdin()
            .lock()
            .read_line(&mut value)
            .map_err(|_| "init: input could not be read")?;
        if length == 0 {
            return Err("init: input ended before setup completed");
        }
        Ok(value.trim_end_matches(['\r', '\n']).to_owned())
    }
}

fn read_secret(input: &mut impl Read) -> Result<String> {
    let mut value = Vec::new();
    loop {
        let mut byte = [0];
        input
            .read_exact(&mut byte)
            .map_err(|_| "init: secret input ended before setup completed")?;
        match byte[0] {
            b'\r' | b'\n' => {
                return String::from_utf8(value).map_err(|_| "init: secret input must be UTF-8");
            }
            3 | 4 => return Err("init: setup cancelled"),
            8 | 127 => {
                value.pop();
            }
            byte => {
                value.push(byte);
                if value.len() > 4096 {
                    return Err("init: secret input exceeds 4096 bytes");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_editing_and_cancellation_do_not_echo_input() {
        for end in *b"\r\n" {
            let input = [b'a', b'b', 8, b'c', 127, b'd', end];
            assert_eq!(read_secret(&mut input.as_slice()).unwrap(), "ad");
        }
        assert_eq!(read_secret(&mut b"\x08\x7fx\n".as_slice()).unwrap(), "x");
        for input in [b"secret\x03".as_slice(), b"secret\x04", b"end", b"\xff\n"] {
            assert!(read_secret(&mut &*input).is_err());
        }
        let input = format!("{}\n", "x".repeat(4096));
        assert_eq!(read_secret(&mut input.as_bytes()).unwrap().len(), 4096);
        let input = format!("{}\n", "x".repeat(4097));
        assert!(read_secret(&mut input.as_bytes()).is_err());
    }
}
