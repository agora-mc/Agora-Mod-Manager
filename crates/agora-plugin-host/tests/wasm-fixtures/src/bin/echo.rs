use std::io::{Read, Write};

fn main() {
    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        std::process::exit(2);
    }
    if std::io::stdout().write_all(&input).is_err() {
        std::process::exit(3);
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().write_all(b"echo: done\n");
}
