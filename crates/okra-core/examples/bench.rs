//! Micro-benchmark for the hot per-keystroke paths: `cargo run --release -p okra-core --example bench`
use okra_core::matcher::{fuzzy, is_subsequence, normalize};
use std::time::Instant;

fn lcg(seed: &mut u64) -> u64 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *seed >> 33
}

fn synth_name(seed: &mut u64) -> String {
    const WORDS: &[&str] = &[
        "report", "invoice", "final", "draft", "thesis", "turbine", "heat", "transfer", "chapter",
        "notes", "Visual", "Studio", "Code", "Chrome", "Firefox", "setup", "backup", "photo",
        "IMG", "scan", "lecture", "design", "gear", "shaft", "fluent", "ansys", "model", "v2",
        "copy", "new",
    ];
    const EXT: &[&str] = &[
        "pdf", "docx", "xlsx", "txt", "png", "py", "rs", "dll", "exe", "zip",
    ];
    let n = 1 + (lcg(seed) % 4) as usize;
    let mut s = String::new();
    for i in 0..n {
        if i > 0 {
            s.push(['_', '-', ' '][(lcg(seed) % 3) as usize]);
        }
        s.push_str(WORDS[(lcg(seed) % WORDS.len() as u64) as usize]);
    }
    s.push('.');
    s.push_str(EXT[(lcg(seed) % EXT.len() as u64) as usize]);
    s
}

fn main() {
    let mut seed = 42u64;
    let apps: Vec<String> = (0..400).map(|_| synth_name(&mut seed)).collect();
    let files: Vec<String> = (0..300_000).map(|_| synth_name(&mut seed)).collect();
    let files_lower: Vec<String> = files.iter().map(|s| s.to_lowercase()).collect();
    let paths: Vec<String> = (0..200)
        .map(|_| {
            format!(
                "C:\\Users\\student\\Documents\\University\\Semester7\\{}\\{}",
                synth_name(&mut seed),
                synth_name(&mut seed)
            )
        })
        .collect();
    let queries = [
        "ch",
        "chr",
        "chrome",
        "vsc",
        "turb",
        "heat tr",
        "report fin",
        "invoice pdf",
    ];

    // 1. apps-style: fuzzy over 400 names (what AppsProvider does per keystroke)
    let t = Instant::now();
    let mut hits = 0usize;
    let reps = 200;
    for _ in 0..reps {
        for q in &queries {
            for a in &apps {
                if fuzzy(q, a).is_some() {
                    hits += 1;
                }
            }
        }
    }
    let per = t.elapsed().as_secs_f64() * 1e6 / (reps * queries.len()) as f64;
    println!("apps  (400 names)         : {per:8.1} us / query   (hits {hits})");

    // 2. files-style rescoring of Everything's 200 hits by path (worst case: name misses)
    let t = Instant::now();
    let reps = 50;
    let mut hits = 0usize;
    for _ in 0..reps {
        for q in &queries {
            for p in &paths {
                if fuzzy(q, p).is_some() {
                    hits += 1;
                }
            }
        }
    }
    let per = t.elapsed().as_secs_f64() * 1e6 / (reps * queries.len()) as f64;
    println!("path rescoring (200 paths): {per:8.1} us / query   (hits {hits})");

    // 3. built-in index style: subsequence prefilter + fuzzy over 300k names
    let t = Instant::now();
    let mut hits = 0usize;
    for q in &queries {
        let qc = normalize(q);
        for (name, lower) in files.iter().zip(&files_lower) {
            if is_subsequence(&qc, lower) && fuzzy(q, name).is_some() {
                hits += 1;
            }
        }
    }
    let per = t.elapsed().as_secs_f64() * 1e3 / queries.len() as f64;
    println!("built-in index (300k)     : {per:8.2} ms / query   (hits {hits})");
}
