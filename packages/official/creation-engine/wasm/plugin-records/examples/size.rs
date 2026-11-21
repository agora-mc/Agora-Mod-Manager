use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let bytes = std::fs::read(&path).expect("read");
    let name = std::path::Path::new(&path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let start = Instant::now();
    match plugin_records::parse_plugin(&name, &bytes) {
        Ok(index) => {
            let serialized = index.serialize();
            let (keys, _) = plugin_records::build_overlaps(std::slice::from_ref(&index));
            println!(
                "{}: bytes={} records={} strings={} index={} keys={} parse_ms={}",
                name,
                bytes.len(),
                index.records.len(),
                index.strings.len(),
                serialized.len(),
                keys.len(),
                start.elapsed().as_millis()
            );
        }
        Err(error) => println!("{}: ERROR {}", name, error),
    }
}
