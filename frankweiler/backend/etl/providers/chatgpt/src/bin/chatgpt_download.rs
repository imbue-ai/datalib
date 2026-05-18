//! Stub for `chatgpt-download`. Implemented at Stage 5 of the porting
//! plan. Exists today so the crate's `[[bin]]` entry resolves and the
//! Bazel `rust_binary` target compiles.

fn main() {
    eprintln!("chatgpt-download: not implemented yet (porting in progress)");
    std::process::exit(1);
}
