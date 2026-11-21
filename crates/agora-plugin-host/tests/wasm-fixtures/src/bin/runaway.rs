fn main() {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    loop {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        std::hint::black_box(state);
    }
}
