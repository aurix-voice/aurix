//! Regenerate `include/aurix_client.h` from the `ffi` module.
//!
//! ```text
//! cargo run -p aurix-client --example gen_header
//! ```

use std::path::PathBuf;

fn main() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let header = aurix_client_header::generate(&crate_dir);
    let out = crate_dir.join("include").join("aurix_client.h");
    std::fs::write(&out, header).expect("write header");
    println!("wrote {}", out.display());
}

mod aurix_client_header {
    include!("../tests/support/header.rs");
}
