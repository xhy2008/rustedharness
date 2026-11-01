//! Optional Tauri 2 desktop shell.
//!
//! The desktop build starts the same axum/WebSocket server as `rustedharness
//! serve` on an ephemeral loopback port and points a native webview at it. The
//! UI therefore stays byte-identical to the browser frontend, while the Rust
//! core still owns the filesystem, processes and model calls.
//!
//! Build with `cargo run -p rustedharness-gui --features desktop`. That needs
//! the platform webview toolchain (webkit2gtk on Linux, WebView2 on Windows,
//! WKWebView on macOS), which is why the feature is off by default.

#[cfg(feature = "desktop")]
mod desktop {
    use std::sync::mpsc;

    use rustedharness_core::config::Config;

    /// Start the HTTP server on 127.0.0.1:0 in a dedicated thread and return
    /// the UI URL. The thread keeps the runtime alive for the app lifetime.
    fn start_embedded_server() -> Result<String, String> {
        let config = Config::load().map_err(|err| err.to_string())?;
        let cwd = std::env::current_dir().map_err(|err| err.to_string())?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = tx.send(Err(format!("cannot start tokio runtime: {err}")));
                    return;
                }
            };
            runtime.block_on(async move {
                match rustedharness_web::spawn(config, cwd, "127.0.0.1".to_string(), 0, None).await
                {
                    Ok((addr, server)) => {
                        let _ = tx.send(Ok(format!("http://{addr}/")));
                        // Keep the server alive until the process exits. Tauri
                        // owns the main thread, so this thread only supervises.
                        let _ = server.await;
                    }
                    Err(err) => {
                        let _ = tx.send(Err(format!("cannot start web server: {err}")));
                    }
                }
            });
        });
        rx.recv()
            .map_err(|err| format!("server thread failed: {err}"))?
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let url = start_embedded_server().map_err(|err| -> Box<dyn std::error::Error> {
            Box::new(std::io::Error::other(err))
        })?;
        let parsed: tauri::Url = url.parse()?;
        tauri::Builder::default()
            .setup(move |app| {
                tauri::WebviewWindowBuilder::new(
                    app,
                    "main",
                    tauri::WebviewUrl::External(parsed.clone()),
                )
                .title("RustedHarness")
                .inner_size(1200.0, 820.0)
                .min_inner_size(640.0, 480.0)
                .build()?;
                Ok(())
            })
            .run(tauri::generate_context!())?;
        Ok(())
    }
}

#[cfg(feature = "desktop")]
pub use desktop::run;
