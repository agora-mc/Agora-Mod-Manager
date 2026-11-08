use std::io::{Read, Write};

fn main() {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        std::process::exit(2);
    }
    let mut out = std::io::stdout();
    for line in input.lines() {
        let mut parts = line.splitn(2, ' ');
        let command = parts.next().unwrap_or("");
        let argument = parts.next().unwrap_or("");
        match command {
            "list" => match std::fs::read_dir(argument) {
                Ok(entries) => {
                    let mut names: Vec<String> = entries
                        .flatten()
                        .map(|entry| entry.file_name().to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    let _ = writeln!(out, "LIST:{}", names.join(","));
                }
                Err(error) => {
                    let _ = writeln!(out, "LIST_ERR:{error}");
                }
            },
            "write" => match std::fs::write(argument, b"x") {
                Ok(()) => {
                    let _ = writeln!(out, "WROTE");
                }
                Err(_) => {
                    let _ = writeln!(out, "WRITE_FAILED");
                }
            },
            "open" => match std::fs::File::open(argument) {
                Ok(_) => {
                    let _ = writeln!(out, "OPENED");
                }
                Err(_) => {
                    let _ = writeln!(out, "OPEN_FAILED");
                }
            },
            "read" => match std::fs::read(argument) {
                Ok(bytes) => {
                    let _ = writeln!(out, "READ:{}", String::from_utf8_lossy(&bytes));
                }
                Err(_) => {
                    let _ = writeln!(out, "READ_FAILED");
                }
            },
            _ => {
                let _ = writeln!(out, "UNKNOWN:{command}");
            }
        }
    }
}
