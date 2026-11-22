//! Live LSP latency probe.
//!
//! Requires `rust-analyzer` on PATH. The benchmark is best-effort: when no
//! server is available it prints a skip message and exits successfully.
//!
//! ```sh
//! cargo bench -p rustedharness-core --bench lsp_latency
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustedharness_core::lsp::{LspHub, LspOp, LspQuery};

fn project(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root.join("src")).expect("src dir");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("Cargo.toml");
    let lib = root.join("src/lib.rs");
    std::fs::write(
        &lib,
        concat!(
            "pub fn helper() -> u32 {\n    7\n}\n\n",
            "pub fn caller() -> u32 {\n    helper()\n}\n"
        ),
    )
    .expect("lib.rs");
    lib
}

fn main() {
    if std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skip: rust-analyzer is not on PATH");
        return;
    }
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(run());
}

async fn run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().join("probe");
    let lib = project(&cwd);
    let config = rustedharness_core::config::Config::default();
    let hub = LspHub::new(cwd, &config.lsp, None);
    let query = |path: PathBuf| LspQuery {
        op: LspOp::Definition,
        server: None,
        path,
        line: 6,
        column: 5,
        symbol: None,
        apply: false,
        timeout_ms: 15_000,
        limit: 20,
    };

    let started = Instant::now();
    let mut cold = None;
    while started.elapsed() < Duration::from_secs(120) {
        let answer = hub.answer(query(lib.clone())).await.expect("lsp answer");
        if answer.match_kind == "lsp-precise" && !answer.text.contains("no matches") {
            cold = Some(started.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let Some(cold) = cold else {
        eprintln!("skip: rust-analyzer never produced a precise answer");
        hub.shutdown_all().await;
        return;
    };
    println!("cold: first precise answer after {cold:?}");

    let mut warm = Vec::new();
    for _ in 0..10 {
        let at = Instant::now();
        let answer = hub.answer(query(lib.clone())).await.expect("lsp answer");
        assert_eq!(answer.match_kind, "lsp-precise", "{}", answer.text);
        warm.push(at.elapsed());
    }
    warm.sort();
    println!(
        "warm: median={:?} p90={:?}",
        warm[warm.len() / 2],
        warm[warm.len() * 9 / 10]
    );
    hub.shutdown_all().await;
}
