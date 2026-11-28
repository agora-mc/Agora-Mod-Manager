use std::io::Read;

fn main() {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let millis: u64 = input.trim().parse().unwrap_or(3000);
    std::thread::sleep(std::time::Duration::from_millis(millis));
    let _ = std::io::Write::write_all(&mut std::io::stdout(), b"woke\n");
}
