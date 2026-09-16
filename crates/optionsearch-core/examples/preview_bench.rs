//! Times [`optionsearch_core::preview`] on real files.
//!
//! ```text
//! cargo run --release --example preview_bench -- FILE...
//! ```

use std::time::Instant;

use optionsearch_core::{Preview, PreviewLimits, preview};

fn main() {
    let limits = PreviewLimits::default();
    for arg in std::env::args().skip(1) {
        let path = std::path::PathBuf::from(&arg);
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        // Twice: the second run shows what a cache hit costs.
        let cold = Instant::now();
        let p = preview(&path, &limits);
        let cold = cold.elapsed();
        let warm = Instant::now();
        let _ = preview(&path, &limits);
        let warm = warm.elapsed();
        println!(
            "{:>10} KB  cold {:>10.3?}  warm {:>10.3?}  {}  {}",
            size / 1024,
            cold,
            warm,
            describe(&p),
            path.display()
        );
    }
}

fn describe(p: &Preview) -> String {
    match p {
        Preview::Text {
            content,
            truncated,
            lines,
        } => format!(
            "text {} B, {lines} lines, truncated={truncated}",
            content.len()
        ),
        Preview::Image { width, height, .. } => format!("image {width}x{height}"),
        Preview::Pdf { text, pages, .. } => format!("pdf {pages} pages, {} B text", text.len()),
        Preview::Audio { .. } => "audio".to_string(),
        Preview::Meta(m) => format!("meta {}", m.kind),
        Preview::Error(e) => format!("error {e}"),
    }
}
