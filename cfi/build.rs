fn main() {
    // Tell cargo to re-run if linker scripts change
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=link.x");
    println!("cargo:rerun-if-changed=build.rs");

    // Add the crate root to the linker search path and pass linker scripts.
    // These used to live in .cargo/config.toml rustflags, but in a workspace
    // each crate needs its own linker scripts so we emit them from build.rs.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search=native={}", manifest_dir);
    println!("cargo:rustc-link-arg=-T{}/memory.x", manifest_dir);
    println!("cargo:rustc-link-arg=-T{}/link.x", manifest_dir);
    println!("cargo:rustc-link-arg=--no-relax");
}
