//! Live language-server probe.
//!
//! Run with the server you want to exercise already installed:
//!
//! ```sh
//! cargo test -p rustedharness-core --test lsp_live -- --ignored --nocapture
//! ```
//!
//! It builds a throwaway cargo project, asks rust-analyzer for the definition
//! of a call site, and asserts the answer came from the server (`lsp-precise`)
//! rather than the name-based fallback. Failing early with `server said: ...`
//! means the binary is there but cannot run - typically a rustup shim whose
//! component was never installed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustedharness_core::lsp::{LspHub, LspOp, LspQuery};

fn project(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let lib = root.join("src/lib.rs");
    std::fs::write(
        &lib,
        concat!(
            "pub fn helper() -> u32 {\n    7\n}\n\n",
            "pub fn caller() -> u32 {\n    helper()\n}\n"
        ),
    )
    .unwrap();
    lib
}

#[tokio::test]
#[ignore = "needs rust-analyzer on PATH; run on demand"]
async fn rust_analyzer_answers_a_definition() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("probe");
    let lib = project(&cwd);
    let config = rustedharness_core::config::Config::default();
    let hub = LspHub::new(cwd.clone(), &config.lsp, None);

    // `helper()` on line 6, column 5 (1-based).
    let query = || LspQuery {
        op: LspOp::Definition,
        server: None,
        path: lib.clone(),
        line: 6,
        column: 5,
        symbol: None,
        apply: false,
        timeout_ms: 15_000,
        limit: 20,
    };

    let started = Instant::now();
    let mut answer = None;
    while started.elapsed() < Duration::from_secs(120) {
        let result = hub.answer(query()).await.unwrap();
        println!(
            "[{:>6}ms] {} {:?}",
            started.elapsed().as_millis(),
            result.match_kind,
            result.warmup_hint
        );
        // A server that is still loading answers `no matches`; keep asking.
        if result.match_kind == "lsp-precise" && !result.text.contains("no matches") {
            answer = Some(result);
            break;
        }
        for status in hub.statuses().iter().filter(|s| s.language == "rust") {
            println!("   {} {:?}", status.state, status.detail);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let answer = answer.expect("rust-analyzer never produced a precise answer");
    println!(
        "answer: {}\nfirst precise answer after {:?}",
        answer.text,
        started.elapsed()
    );
    assert!(
        answer.text.contains("src/lib.rs:1"),
        "definition should land on line 1: {}",
        answer.text
    );

    // The status line relies on this: "warming" during load, "running" after.
    let state = hub
        .statuses()
        .into_iter()
        .find(|status| status.language == "rust")
        .map(|status| status.state)
        .unwrap_or_default();
    println!("rust state after warm-up: {state}");
    assert_eq!(state, "running");
    hub.shutdown_all().await;
}

/// `op = "restart"` replaces the live server and it answers again afterwards.
#[tokio::test]
#[ignore = "needs rust-analyzer on PATH; takes ~30s on purpose"]
async fn restart_replaces_the_server_and_it_answers_again() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("probe");
    let lib = project(&cwd);
    let config = rustedharness_core::config::Config::default();
    let hub = LspHub::new(cwd.clone(), &config.lsp, None);

    let query = || LspQuery {
        op: LspOp::Definition,
        server: None,
        path: lib.clone(),
        line: 6,
        column: 5,
        symbol: None,
        apply: false,
        timeout_ms: 15_000,
        limit: 20,
    };
    let wait_ready = || async {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(120) {
            let answer = hub.answer(query()).await.unwrap();
            if answer.match_kind == "lsp-precise" {
                return started.elapsed();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        panic!("server never became ready");
    };
    println!("first start: {:?}", wait_ready().await);

    let restarted = hub
        .answer(LspQuery {
            op: LspOp::Restart,
            server: Some("rust-analyzer".into()),
            path: std::path::PathBuf::new(),
            line: 1,
            column: 1,
            symbol: None,
            apply: false,
            timeout_ms: 0,
            limit: 1,
        })
        .await
        .unwrap();
    println!("restart: {} ({})", restarted.text, restarted.match_kind);
    assert_eq!(restarted.match_kind, "lsp-maintenance");
    assert!(restarted.text.starts_with("restarted rust-analyzer"));
    assert!(wait_ready().await < Duration::from_secs(120));
    hub.shutdown_all().await;
}
