fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");

    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out = std::path::Path::new(&crate_dir).join("../include/conmon_log_plugin.h");

    // Ensure include/ exists
    std::fs::create_dir_all(out.parent().unwrap()).expect("include/ directory does not exist");

    // Use cbindgen as a build-dependency (no CLI needed)
    let config = cbindgen::Config::from_file(format!("{crate_dir}/cbindgen.toml"))
        .expect("read cbindgen.toml");
    let bindings = cbindgen::generate_with_config(&crate_dir, config).expect("cbindgen generate");
    bindings.write_to_file(&out);
}
