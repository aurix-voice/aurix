//! The committed header must match what cbindgen produces from `src/ffi.rs`, and the C sample
//! must compile against it and link with the cdylib (skipped when no C compiler is present).

use std::path::{Path, PathBuf};
use std::process::Command;

mod support {
    pub mod header;
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate_dir().join("../../target"))
}

#[test]
fn committed_header_is_up_to_date() {
    let generated = support::header::generate(&crate_dir());
    let committed = std::fs::read_to_string(crate_dir().join("include/aurix_client.h"))
        .expect("include/aurix_client.h is committed")
        .replace("\r\n", "\n");
    assert!(
        generated == committed,
        "include/aurix_client.h is stale; run `cargo run -p aurix-client --example gen_header`"
    );
}

fn find_cc() -> Option<String> {
    if let Ok(cc) = std::env::var("CC") {
        return Some(cc);
    }
    ["cc", "clang", "gcc"]
        .into_iter()
        .find(|c| Command::new(c).arg("--version").output().is_ok())
        .map(str::to_string)
}

fn cdylib_path(target: &Path) -> PathBuf {
    let name = if cfg!(target_os = "windows") {
        "aurix_client.dll"
    } else if cfg!(target_os = "macos") {
        "libaurix_client.dylib"
    } else {
        "libaurix_client.so"
    };
    target.join("debug").join(name)
}

#[test]
fn c_sample_compiles_links_and_reports_connect_failure() {
    let Some(cc) = find_cc() else {
        eprintln!("no C compiler found; skipping");
        return;
    };
    let target = target_dir();
    let lib = cdylib_path(&target);
    if !lib.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "-p", "aurix-client", "--lib"])
            .env("CARGO_TARGET_DIR", &target)
            .current_dir(crate_dir())
            .status()
            .expect("run cargo build");
        assert!(status.success(), "cargo build -p aurix-client failed");
    }
    assert!(lib.exists(), "cdylib not found at {}", lib.display());

    let out_dir = target.join("aurix-client-c-sample");
    std::fs::create_dir_all(&out_dir).unwrap();
    let exe = out_dir.join("voice_loop");
    let compile = Command::new(&cc)
        .arg(crate_dir().join("examples/c/voice_loop.c"))
        .arg("-std=c99")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg("-I")
        .arg(crate_dir().join("include"))
        .arg("-L")
        .arg(lib.parent().unwrap())
        .args(["-laurix_client", "-lpthread", "-lm", "-o"])
        .arg(&exe)
        .output()
        .expect("run C compiler");
    assert!(
        compile.status.success(),
        "C sample failed to compile:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let mut run = Command::new(&exe);
    run.args([
        "ws://127.0.0.1:1/ws",
        "not-a-real-token",
        "00000000-0000-0000-0000-000000000001",
        "1",
    ]);
    let lib_dir = lib.parent().unwrap();
    run.env("LD_LIBRARY_PATH", lib_dir)
        .env("DYLD_LIBRARY_PATH", lib_dir);
    let output = run.output().expect("run C sample");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "expected connect failure exit code 3\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("aurix_client "), "{stdout}");
    assert!(stderr.contains("connect failed:"), "{stderr}");
}
