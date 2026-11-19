// blake3 must be built with its SIMD kernels, which are C and assembly that
// its build script compiles. Its `pure` feature, or a build that finds no C
// compiler, leaves them out without a word, and every hash then runs at
// about half speed (docs/dev/history.md). This file then fails to link.

use blake3 as _;

#[cfg(target_arch = "aarch64")]
extern "C" {
    #[link_name = "blake3_hash_many_neon"]
    fn simd_kernel();
}

#[cfg(target_arch = "x86_64")]
extern "C" {
    #[link_name = "blake3_hash_many_avx512"]
    fn simd_kernel();
}

/// Guards against blake3 losing its NEON or AVX-512 code, as it did when
/// `pure` crept back into Cargo.toml.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn blake3_links_its_simd_kernel() {
    let kernel: unsafe extern "C" fn() = simd_kernel;
    std::hint::black_box(kernel);
}
