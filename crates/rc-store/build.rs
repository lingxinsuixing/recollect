fn main() {
    if let Ok(t) = std::env::var("TARGET") {
        println!("cargo:rustc-env=TARGET={t}");
    }
    println!("cargo:rerun-if-env-changed=TARGET");
}
