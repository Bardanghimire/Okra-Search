//! Headless REPL for the engine: `cargo run -p okra-core --example cli -- "query"`
use okra_core::{Config, Engine};

fn main() {
    let mut cfg = Config::default();
    if let Ok(root) = std::env::var("FS_ROOT") {
        cfg.file_roots = vec![root.into()];
    }
    let engine = Engine::new(&cfg);
    std::thread::sleep(std::time::Duration::from_millis(1500)); // let the file index warm up
    for q in std::env::args().skip(1) {
        println!("== {q}");
        for it in engine.search(&q).iter().take(6) {
            println!(
                "  [{:>5}] {:<10} {}  ({})",
                it.score,
                it.kind.label(),
                it.title,
                it.subtitle
            );
        }
    }
}
