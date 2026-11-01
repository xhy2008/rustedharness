fn main() {
    #[cfg(feature = "desktop")]
    {
        if let Err(err) = rustedharness_gui::run() {
            eprintln!("failed to start RustedHarness desktop: {err}");
            std::process::exit(1);
        }
    }
    #[cfg(not(feature = "desktop"))]
    {
        eprintln!(
            "rustedharness-gui was built without the `desktop` feature.\n\
             Use `cargo run -p rustedharness-gui --features desktop`, or use the\n\
             TUI (`rustedharness`) or web UI (`rustedharness serve`)."
        );
    }
}
