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

    // Warm latency: what a query costs once the crate is loaded.
    let warm = Instant::now();
    let mut samples = Vec::new();
    for _ in 0..10 {
        let at = Instant::now();
        let result = hub.answer(query()).await.unwrap();
        assert_eq!(result.match_kind, "lsp-precise", "{}", result.text);
        samples.push(at.elapsed());
    }
    samples.sort();
    println!(
        "10 warm definitions: median {:?} p90 {:?} total {:?}",
        samples[5],
        samples[8],
        warm.elapsed()
    );
    hub.shutdown_all().await;
}

/// The startup warm-up exists so the first `lsp` call does not wait for
/// rust-analyzer to load the crate: by the time a user asks, the server should
/// already be ready and the query should cost milliseconds.
#[tokio::test]
#[ignore = "needs rust-analyzer on PATH; takes ~20s on purpose"]
async fn prewarm_takes_the_cold_start_off_the_critical_path() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("probe");
    let lib = project(&cwd);
    let config = rustedharness_core::config::Config::default();
    let hub = LspHub::new(cwd.clone(), &config.lsp, None);

    // LspHub::new schedules the warm-up 500ms in; give rust-analyzer the time
    // it needs without ever asking it a question.
    tokio::time::sleep(Duration::from_secs(20)).await;

    let at = Instant::now();
    let answer = hub
        .answer(LspQuery {
            op: LspOp::Definition,
            path: lib,
            line: 6,
            column: 5,
            symbol: None,
            apply: false,
            timeout_ms: 15_000,
            limit: 20,
        })
        .await
        .unwrap();
    let elapsed = at.elapsed();
    println!("first query after warm-up: {elapsed:?} {}", answer.text);
    assert_eq!(answer.match_kind, "lsp-precise", "{}", answer.text);
    assert!(
        elapsed < Duration::from_secs(2),
        "cold start is still on the critical path: {elapsed:?}"
    );
    hub.shutdown_all().await;
}
