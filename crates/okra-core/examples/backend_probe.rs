//! Show which file backend answers each query, from an ordinary (unelevated) process, using the
//! same backend chain as the launcher (Everything, then the NTFS helper, then the walker).
//!
//!     cargo run -p okra-core --example backend_probe -- cargo okra
use okra_core::providers::files::FilesProvider;
use okra_core::{Config, Provider};
use std::time::Instant;

fn main() {
    let files = FilesProvider::new(&Config::default());
    if let Some(notice) = files.startup_notice() {
        println!("notice: {notice}");
    }
    println!("{}", files.helper_status().describe());
    let queries: Vec<String> = std::env::args().skip(1).collect();
    for q in if queries.is_empty() { vec!["cargo".to_string()] } else { queries } {
        let start = Instant::now();
        let items = files.query(&q, 10);
        println!(
            "{q:?}: {} item(s) in {:.1} ms, answered by: {}",
            items.len(),
            start.elapsed().as_secs_f64() * 1000.0,
            files.active_backend()
        );
    }
}
