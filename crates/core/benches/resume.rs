//! Resume benchmark.
//!
//! Builds a synthetic 800-turn transcript once, then times session loading and
//! `Agent::resume` separately. Run with:
//!
//! ```sh
//! cargo bench -p rustedharness-core --bench resume
//! ```

use std::time::Instant;

use rustedharness_core::agent::Agent;
use rustedharness_core::config::Config;
use rustedharness_core::provider::ChatMessage;
use rustedharness_core::session::{SessionStore, SessionWriter};

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::default();
    config.session.dir = Some(dir.path().join("sessions"));
    config.session.max_saved_sessions = 1;
    config.lsp.enabled = false;
    config.mcp.enabled = false;
    std::fs::create_dir_all(config.session.dir.as_ref().unwrap()).expect("session dir");

    let body = "fn main() {\n    println!(\"hello\");\n}\n".repeat(20);
    let mut writer =
        SessionWriter::create(&config.session, dir.path(), "agent", "compact").expect("writer");
    for turn in 0..800 {
        writer.user(&format!("turn {turn}")).expect("user");
        writer
            .assistant(&ChatMessage::assistant("Looking."))
            .expect("assistant");
        writer
            .tool_result(&format!("call_{turn}"), "read", true, &body)
            .expect("tool result");
    }
    let id = writer.id().to_string();
    let path = writer.path().to_path_buf();
    drop(writer);
    let size = std::fs::metadata(&path).expect("metadata").len();
    println!(
        "transcript: {:.1} MiB, 800 turns",
        size as f64 / 1_048_576.0
    );

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let store = SessionStore::new(&config.session);
    for run in 1..=3 {
        let started = Instant::now();
        let loaded = store.load(&id).expect("load");
        let load = started.elapsed();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let started = Instant::now();
        let agent = runtime
            .block_on(Agent::resume(config.clone(), &id, None, tx))
            .expect("resume");
        let resume = started.elapsed();
        runtime.block_on(agent.shutdown());

        println!(
            "run {run}: load={load:?} resume={resume:?} messages={}",
            loaded.messages.len()
        );
    }
}
