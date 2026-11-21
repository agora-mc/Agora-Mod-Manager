fn main() {
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut chunk = Vec::with_capacity(1024 * 1024);
        chunk.resize(1024 * 1024, 0x5a);
        chunks.push(chunk);
        std::hint::black_box(&chunks);
    }
}
