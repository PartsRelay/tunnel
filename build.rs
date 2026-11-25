fn main() {
    // The binary downloads the archive of its own target
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=TARGET={target}");
}
