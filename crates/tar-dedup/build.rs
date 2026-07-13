fn main() {
    // Bundled liblzma (xz2 static) uses POSIX threads for `-T` / MtStreamBuilder.
    #[cfg(unix)]
    println!("cargo:rustc-link-lib=pthread");
}
