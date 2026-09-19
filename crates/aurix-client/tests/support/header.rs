use std::path::Path;

/// Run cbindgen on the crate and return the header text (normalised line endings).
pub fn generate(crate_dir: &Path) -> String {
    let config =
        cbindgen::Config::from_file(crate_dir.join("cbindgen.toml")).expect("read cbindgen.toml");
    let bindings = cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
        .expect("cbindgen generate");
    let mut out = Vec::new();
    bindings.write(&mut out);
    String::from_utf8(out)
        .expect("utf8 header")
        .replace("\r\n", "\n")
}
