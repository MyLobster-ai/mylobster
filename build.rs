fn main() {
    #[cfg(feature = "ffi")]
    {
        let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let output_dir = std::path::Path::new(&crate_dir).join("include");
        std::fs::create_dir_all(&output_dir).expect("failed to create include/ directory");

        let config =
            cbindgen::Config::from_file("cbindgen.toml").expect("failed to read cbindgen.toml");

        cbindgen::Builder::new()
            .with_crate(&crate_dir)
            .with_config(config)
            .generate()
            .expect("failed to generate C header")
            .write_to_file(output_dir.join("mylobster.h"));
    }
}
