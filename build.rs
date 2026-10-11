fn main() {
    // The target triple, so `update` picks the matching release binary.
    println!(
        "cargo:rustc-env=OPENCLAW_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
}
