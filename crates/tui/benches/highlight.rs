//! Syntax-highlighting benchmark.
//!
//! ```sh
//! cargo bench -p rustedharness-tui --bench highlight
//! ```

use std::time::{Duration, Instant};

fn main() {
    let unit = "pub const TEXT: Color = Color::Rgb(0xe4, 0xe5, 0xea); // reading colour\n";
    for count in [100usize, 400, 1_600] {
        let source = unit.repeat(count);
        let mut samples = Vec::with_capacity(20);
        for _ in 0..20 {
            let started = Instant::now();
            let lines = rustedharness_tui::highlight(&source, "rust");
            samples.push(started.elapsed());
            std::hint::black_box(lines);
        }
        samples.sort();
        println!(
            "{count} lines: median={:?} p90={:?} max={:?}",
            samples[samples.len() / 2],
            samples[samples.len() * 9 / 10],
            samples.last().copied().unwrap_or(Duration::ZERO)
        );
    }
}
