use std::io::Write;

fn main() {
    let environment: Vec<(String, String)> = std::env::vars().collect();
    let arguments: Vec<String> = std::env::args().collect();
    let mut out = std::io::stdout();
    let _ = writeln!(out, "ENV:{}", environment.len());
    for (name, value) in environment {
        let _ = writeln!(out, "ENV={name}={value}");
    }
    let _ = writeln!(out, "ARGS:{}", arguments.len());
    for argument in arguments {
        let _ = writeln!(out, "ARG={argument}");
    }
}
