//! WASI entry point: one JSON request on stdin, one JSON reply on stdout.

use std::io::{Read, Write};

fn main() {
    let mut request = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut request) {
        fail(&format!("the request cannot be read: {error}"));
    }

    match loot_sort::run_request(&request) {
        Ok(reply) => {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            if let Err(error) = stdout
                .write_all(reply.as_bytes())
                .and_then(|()| stdout.flush())
            {
                fail(&format!("the reply cannot be written: {error}"));
            }
        }
        Err(error) => fail(&error),
    }
}

fn fail(message: &str) -> ! {
    let mut reply = String::from("{\"error\":");
    loot_sort::json::write_string(message, &mut reply);
    reply.push_str("}\n");
    let _ = std::io::stdout().write_all(reply.as_bytes());
    std::process::exit(1);
}
