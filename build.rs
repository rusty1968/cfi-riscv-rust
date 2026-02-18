fn main() {
    // Tell cargo to re-run if linker scripts change
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=link.x");
    println!("cargo:rerun-if-changed=build.rs");

    // Add the project root to the linker search path so -Tmemory.x and
    // -Tlink.x resolve correctly
    let out_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search=native={}", out_dir);
}
