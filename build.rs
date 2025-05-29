fn main() {
    if let Ok(path) = std::env::var("LIBTORCH") {
        println!("cargo:rustc-link-search=native={}/lib", path);
    }
}
