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

fn find_compiler(env_var: &str, candidates: &[&str]) -> Option<String> {
    if let Ok(cc) = std::env::var(env_var) {
        return Some(cc);
    }
    candidates
        .iter()
        .copied()
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

fn ensure_cdylib(target: &Path) -> PathBuf {
    let lib = cdylib_path(target);
    if !lib.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "-p", "aurix-client", "--lib"])
            .env("CARGO_TARGET_DIR", target)
            .current_dir(crate_dir())
            .status()
            .expect("run cargo build");
        assert!(status.success(), "cargo build -p aurix-client failed");
    }
    assert!(lib.exists(), "cdylib not found at {}", lib.display());
    lib
}

/// Compile `source` with `compiler` against the header and cdylib, run it against a closed
/// port and check that it reports a connect failure the documented way.
fn build_and_run_sample(compiler: &str, std_flag: &str, source: &str, exe_name: &str) {
    let target = target_dir();
    let lib = ensure_cdylib(&target);
    let out_dir = target.join("aurix-client-samples");
    std::fs::create_dir_all(&out_dir).unwrap();
    let exe = out_dir.join(exe_name);
    let compile = Command::new(compiler)
        .arg(crate_dir().join(source))
        .arg(std_flag)
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
        .expect("run compiler");
    assert!(
        compile.status.success(),
        "{source} failed to compile:\n{}",
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
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut search_path = std::env::split_paths(&path).collect::<Vec<_>>();
    search_path.insert(0, lib_dir.to_path_buf());
    run.env("LD_LIBRARY_PATH", lib_dir)
        .env("DYLD_LIBRARY_PATH", lib_dir)
        .env("PATH", std::env::join_paths(search_path).expect("joinable PATH"))
        .env(
            "AURIX_REGIONS_JSON",
            r#"{"regions":[{"region":"eu_west","node_id":"11111111-1111-1111-1111-111111111111","ws_url":"wss://eu1.example/ws","probe_url":"https://eu1.example/health","location":null,"distance_km":null,"nodes":2,"load_factor":0.25}],"recommended":null}"#,
        );
    let output = run.output().expect("run sample");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "expected connect failure exit code 3\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("aurix_client "), "{stdout}");
    if source.ends_with(".cpp") {
        assert!(
            stdout.contains("region eu_west node=11111111-1111-1111-1111-111111111111 ws=wss://eu1.example/ws nodes=2"),
            "{stdout}"
        );
    }
    assert!(stderr.contains("connect failed:"), "{stderr}");
}

#[test]
fn c_sample_compiles_links_and_reports_connect_failure() {
    let Some(cc) = find_compiler("CC", &["cc", "clang", "gcc"]) else {
        eprintln!("no C compiler found; skipping");
        return;
    };
    build_and_run_sample(&cc, "-std=c99", "examples/c/voice_loop.c", "voice_loop_c");
}

#[test]
fn cpp_wrapper_sample_compiles_links_and_reports_connect_failure() {
    let Some(cxx) = find_compiler("CXX", &["c++", "clang++", "g++"]) else {
        eprintln!("no C++ compiler found; skipping");
        return;
    };
    build_and_run_sample(
        &cxx,
        "-std=c++11",
        "examples/cpp/voice_loop.cpp",
        "voice_loop_cpp",
    );
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable plugin source dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_sources(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("h") | Some("hpp") | Some("cpp")
        ) {
            out.push(path);
        }
    }
}

/// Identifiers that start right after `prefix` (the prefix itself is included when
/// `keep_prefix`) and are immediately followed by one of `suffixes`.
fn identifiers<'a>(
    text: &'a str,
    prefix: &str,
    keep_prefix: bool,
    suffixes: &[char],
) -> Vec<&'a str> {
    let mut found = Vec::new();
    for (idx, _) in text.match_indices(prefix) {
        let start = if keep_prefix { idx } else { idx + prefix.len() };
        let rest = &text[start..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if end > 0 && rest[end..].starts_with(suffixes) {
            found.push(&rest[..end]);
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// The Unreal plugin cannot be compiled here (no engine), but every native symbol and C++
/// wrapper method it calls must exist in the committed headers, so ABI drift is caught in CI.
#[test]
fn unreal_plugin_uses_only_existing_abi() {
    let plugin = crate_dir().join("../../sdk/unreal/AurixVoice");
    let header = std::fs::read_to_string(crate_dir().join("include/aurix_client.h")).unwrap();
    let wrapper = std::fs::read_to_string(crate_dir().join("include/aurix_client.hpp")).unwrap();

    let uplugin: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(plugin.join("AurixVoice.uplugin")).unwrap())
            .expect("AurixVoice.uplugin is valid JSON");
    for module in uplugin["Modules"].as_array().expect("Modules array") {
        let name = module["Name"].as_str().expect("module name");
        let build_cs = plugin.join(format!("Source/{name}/{name}.Build.cs"));
        assert!(build_cs.is_file(), "missing {}", build_cs.display());
    }
    assert!(plugin
        .join("Source/ThirdParty/AurixClientLibrary/AurixClientLibrary.Build.cs")
        .is_file());

    let mut sources = Vec::new();
    collect_sources(&plugin.join("Source/AurixVoice"), &mut sources);
    assert!(
        sources.len() >= 8,
        "unexpectedly few plugin sources: {sources:?}"
    );

    let declared_functions = identifiers(&header, "aurix_", true, &['(']);
    let declared_constants = identifiers(&header, "AURIX_", true, &[' ', ',', '\n']);
    let mut checked = 0usize;
    for path in sources {
        let text = std::fs::read_to_string(&path).unwrap();
        for symbol in identifiers(&text, "aurix_", true, &['(']) {
            assert!(
                declared_functions.contains(&symbol),
                "{} calls {symbol}, which include/aurix_client.h does not declare",
                path.display()
            );
            checked += 1;
        }
        for constant in identifiers(&text, "AURIX_", true, &[')', ',', ':', ';', ' ']) {
            assert!(
                declared_constants.contains(&constant),
                "{} uses {constant}, which include/aurix_client.h does not define",
                path.display()
            );
            checked += 1;
        }
        for method in identifiers(&text, "->Client.", false, &['(']) {
            assert!(
                wrapper.contains(&format!(" {method}(")),
                "{} calls aurix::Client::{method}, which include/aurix_client.hpp does not define",
                path.display()
            );
            checked += 1;
        }
        for method in identifiers(&text, "Regions.", false, &['('])
            .into_iter()
            .chain(identifiers(&text, "aurix::Regions::", false, &['(']))
        {
            assert!(
                wrapper.contains(&format!(" {method}(")),
                "{} calls aurix::Regions::{method}, which include/aurix_client.hpp does not define",
                path.display()
            );
            checked += 1;
        }
    }
    eprintln!("verified {checked} native references from the Unreal plugin");
    assert!(checked > 80, "reference scan found only {checked} symbols");
}
