//! Fuzzy matcher: best-alignment subsequence scoring (DP) with bonuses for
//! prefix, word-boundary, camelCase and consecutive matches.
//!
//! Performance notes (this runs once per candidate per keystroke):
//! * the query is normalized once into a [`Query`] instead of once per candidate;
//! * non-matching candidates are rejected by an allocation-free subsequence scan;
//! * the DP uses thread-local scratch buffers, so a match performs no heap allocation;
//! * the "best previous position" search is O(1) per cell (a sliding window plus a running
//!   prefix maximum) instead of a scan over every earlier position, which makes scoring
//!   O(query x text) instead of O(query x text^2). Scores are identical to the naive version.

use std::cell::RefCell;

pub struct Match {
    pub score: i64,
    pub indices: Vec<usize>,
}

#[inline]
pub fn lc(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// 64-bit "which characters occur" signature: a-z -> bits 0..26, 0-9 -> 26..36, everything else
/// shares the remaining bits. If `q` is a subsequence of `t`, then `mask(q) & !mask(t) == 0`, so a
/// single AND rejects most non-matches without touching the text (collisions only cause false
/// positives, never false negatives).
#[inline]
pub fn char_mask(chars: impl Iterator<Item = char>) -> u64 {
    let mut m = 0u64;
    for c in chars {
        let bit = match c {
            'a'..='z' => c as u32 - 'a' as u32,
            '0'..='9' => 26 + (c as u32 - '0' as u32),
            _ => 36 + (c as u32 % 28),
        };
        m |= 1u64 << bit;
    }
    m
}

pub fn normalize(s: &str) -> Vec<char> {
    s.chars().filter(|c| !c.is_whitespace()).map(lc).collect()
}

fn is_sep(c: char) -> bool {
    matches!(c, ' ' | '_' | '-' | '/' | '\\' | '.' | ':')
}

/// Cheap pre-filter: is `q` (already normalized) a subsequence of `text`
/// (already lowercased with `lc`)?
pub fn is_subsequence(q: &[char], text: &str) -> bool {
    let mut i = 0;
    for c in text.chars() {
        if i < q.len() && c == q[i] {
            i += 1;
        }
    }
    i == q.len()
}

/// A query prepared once per search and reused for every candidate.
pub struct Query {
    chars: Vec<char>,
    normalized: String,
    mask: u64,
}

impl Query {
    pub fn new(query: &str) -> Self {
        let chars = normalize(query);
        let normalized = chars.iter().copied().collect();
        let mask = char_mask(chars.iter().copied());
        Self {
            chars,
            normalized,
            mask,
        }
    }
    /// See [`char_mask`]: `entry_mask & query.mask() == query.mask()` is necessary for a match.
    pub fn mask(&self) -> u64 {
        self.mask
    }
    pub fn chars(&self) -> &[char] {
        &self.chars
    }
    /// Lowercased query with whitespace removed, matching the filename normalization rules.
    pub fn normalized(&self) -> &str {
        &self.normalized
    }
    pub fn len(&self) -> usize {
        self.chars.len()
    }
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }
}

const NEG: i64 = i64::MIN / 4;
const NO_PREV: u32 = u32::MAX;

#[derive(Default)]
struct Scratch {
    t: Vec<char>,
    tl: Vec<char>,
    dp: Vec<i64>,
    from: Vec<u32>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

/// Score only (no index vector). Use this on hot paths that just rank candidates.
pub fn score(query: &Query, text: &str) -> Option<i64> {
    run(&query.chars, text, None, None)
}

/// Like [`score`], for callers that already hold `lower` = `text` lowercased with [`lc`]
/// (same number of chars as `text`); saves lowercasing every candidate on every keystroke.
pub fn score_with_lower(query: &Query, text: &str, lower: &str) -> Option<i64> {
    run(&query.chars, text, Some(lower), None)
}

const EXACT_FILENAME_SCORE: i64 = 100_000;
const PREFIX_FILENAME_BONUS: i64 = 50_000;

/// Filename-specific score shared by the NTFS helper and the unprivileged fallback.
/// Exact basename matches beat prefixes, prefixes beat fuzzy matches, and exact files beat
/// same-named directories.
pub fn file_name_score(query: &Query, name: &str, lower: &str, is_dir: bool) -> Option<i64> {
    // Exact names need no dynamic-programming pass. Use a separate score band rather than a
    // small additive bonus: long (but valid) fuzzy names can otherwise accumulate enough
    // boundary bonuses to outrank an exact filename. Windows basename limits keep the fuzzy
    // score comfortably below this band.
    if normalized_name_eq(lower, query.normalized()) {
        return Some(EXACT_FILENAME_SCORE + if is_dir { -3 } else { 3 });
    }

    let mut value = score_with_lower(query, name, lower)?;
    // Prefix matches get their own band, above fuzzy alignments. Query normalization removes
    // whitespace, so check the same normalized view without allocating per candidate.
    if normalized_name_starts_with(lower, query.normalized()) {
        value += PREFIX_FILENAME_BONUS;
    }
    value += if is_dir { -3 } else { 3 };
    Some(value)
}

fn normalized_name_eq(lower: &str, normalized_query: &str) -> bool {
    lower
        .chars()
        .filter(|character| !character.is_whitespace())
        .eq(normalized_query.chars())
}

fn normalized_name_starts_with(lower: &str, normalized_query: &str) -> bool {
    let mut query = normalized_query.chars();
    for character in lower.chars().filter(|character| !character.is_whitespace()) {
        match query.next() {
            Some(expected) if character == expected => {}
            Some(_) => return false,
            None => return true,
        }
    }
    query.next().is_none()
}

/// Score plus the matched character positions.
pub fn fuzzy_with(query: &Query, text: &str) -> Option<Match> {
    let mut indices = Vec::new();
    let score = run(&query.chars, text, None, Some(&mut indices))?;
    Some(Match { score, indices })
}

/// Convenience wrapper that prepares the query on every call. Prefer [`Query`] + [`score`]
/// when matching many candidates against one query.
pub fn fuzzy(query: &str, text: &str) -> Option<Match> {
    fuzzy_with(&Query::new(query), text)
}

/// Upper bound on the score of any candidate that is NOT a normalized prefix match, including
/// the +/-3 file/directory tie-break. The first matched character earns at most 24 (start bonus,
/// no gap) and each later one at most 12 (adjacency) + 8 + 10 (separator boundary). The +20/+40
/// contiguous-prefix and exact bonuses apply only to prefix alignments, so they are excluded.
/// Prefix matches additionally get `PREFIX_FILENAME_BONUS`, so once a result heap holds enough
/// entries at or above this bound, non-prefix candidates can be skipped without scoring.
pub fn non_prefix_score_ceiling(query_len: usize) -> i64 {
    24 + 30 * (query_len.saturating_sub(1) as i64) + 3
}

/// True when the whitespace-free lowercase name starts with the (already normalized) query.
pub fn is_normalized_prefix(lower: &str, query: &Query) -> bool {
    normalized_name_starts_with(lower, query.normalized())
}

thread_local! {
    static SPARSE: RefCell<SparseScratch> = RefCell::new(SparseScratch::default());
}

#[derive(Default)]
struct SparseScratch {
    t: Vec<char>,
    tl: Vec<char>,
    prev: Vec<(u32, i64)>,
    cur: Vec<(u32, i64)>,
}

/// Score-only version of [`run_dp`] that returns exactly the same value.
///
/// It keeps only the positions where the name matches each query character (a sparse row)
/// instead of an m x n table, and it needs no traceback matrix: the +20 contiguous-prefix bonus
/// applies exactly when the best alignment ends at index m-1, because a cell (i, j) with j == i
/// can only be reached from (i-1, i-1), so a best end at m-1 implies the whole diagonal matched.
/// `sparse_score_matches_the_dp` checks equality with `run_dp` on randomized inputs.
fn run_sparse(q: &[char], text: &str, lower: Option<&str>) -> Option<i64> {
    let m = q.len();
    if m == 0 {
        return Some(0);
    }
    // Same allocation-free subsequence rejection as the dynamic program.
    let mut qi = 0;
    let mut n = 0usize;
    match lower {
        Some(lower) => {
            for c in lower.chars() {
                n += 1;
                if qi < m && c == q[qi] {
                    qi += 1;
                }
            }
        }
        None => {
            for c in text.chars() {
                n += 1;
                if qi < m && lc(c) == q[qi] {
                    qi += 1;
                }
            }
        }
    }
    if qi < m || m > n {
        return None;
    }

    SPARSE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let SparseScratch { t, tl, prev, cur } = &mut *guard;
        match lower {
            // Hot path: the NTFS cache always supplies `lower`, and most Windows names are
            // ASCII, so index the bytes directly instead of decoding two Vec<char> per candidate.
            Some(lower) if text.is_ascii() => {
                sparse_core(q, text.as_bytes(), lower.as_bytes(), n, prev, cur)
            }
            _ => {
                t.clear();
                t.extend(text.chars());
                tl.clear();
                match lower {
                    Some(lower) => tl.extend(lower.chars()),
                    None => tl.extend(t.iter().map(|&c| lc(c))),
                }
                debug_assert_eq!(t.len(), tl.len());
                sparse_core(q, t, tl, n, prev, cur)
            }
        }
    })
}

fn sparse_core<T: Copy + Into<char>>(
    q: &[char],
    t: &[T],
    tl: &[T],
    n: usize,
    prev: &mut Vec<(u32, i64)>,
    cur: &mut Vec<(u32, i64)>,
) -> Option<i64> {
    let m = q.len();
    let bonus_at = |j: usize| -> i64 {
        let mut bonus = 8;
        if j == 0 {
            bonus += 16;
        } else {
            let p: char = t[j - 1].into();
            if is_sep(p) {
                bonus += 10;
            } else if p.is_lowercase() && Into::<char>::into(t[j]).is_uppercase() {
                bonus += 8;
            }
        }
        bonus
    };

    prev.clear();
    for (j, &value) in tl.iter().take(n).enumerate() {
        if Into::<char>::into(value) == q[0] {
            prev.push((j as u32, bonus_at(j) - (j as i64).min(10)));
        }
    }
    for (i, &query_char) in q.iter().enumerate().skip(1) {
        cur.clear();
        // `far` walks the previous row once: it accumulates the running maximum over all
        // k <= j - 7 (same capped gap penalty), the smallest k winning ties.
        let mut far_val = NEG;
        let mut far_ptr = 0usize;
        let mut near_ptr = 0usize;
        for (j, &value) in tl.iter().take(n).enumerate().skip(i) {
            if Into::<char>::into(value) != query_char {
                continue;
            }
            while far_ptr < prev.len() && (prev[far_ptr].0 as usize) + 7 <= j {
                let v = prev[far_ptr].1;
                if v > far_val {
                    far_val = v;
                }
                far_ptr += 1;
            }
            let mut best = if far_val != NEG { far_val - 7 } else { NEG };
            // Near predecessors: k in [max(i-1, j-6), j-1], strict improvement.
            let lo = (i - 1).max(j.saturating_sub(6));
            while near_ptr < prev.len() && (prev[near_ptr].0 as usize) < lo {
                near_ptr += 1;
            }
            let mut p = near_ptr;
            while p < prev.len() && (prev[p].0 as usize) < j {
                let (k, v) = prev[p];
                let k = k as usize;
                let gap = if k + 1 == j {
                    12
                } else {
                    -((j - k - 1) as i64).min(6) - 1
                };
                let s = v + gap;
                if s > best {
                    best = s;
                }
                p += 1;
            }
            if best != NEG {
                cur.push((j as u32, best + bonus_at(j)));
            }
        }
        std::mem::swap(prev, cur);
        if prev.is_empty() {
            return None;
        }
    }

    let (mut bj, mut bs) = (usize::MAX, NEG);
    for &(j, v) in prev.iter() {
        if v > bs {
            bs = v;
            bj = j as usize;
        }
    }
    if bj == usize::MAX {
        return None;
    }
    let mut score = bs - (n as i64) / 8;
    if bj == m - 1 {
        score += 20; // contiguous prefix
        if m == n {
            score += 40; // exact
        }
    }
    Some(score)
}

fn run(
    q: &[char],
    text: &str,
    lower: Option<&str>,
    indices_out: Option<&mut Vec<usize>>,
) -> Option<i64> {
    if indices_out.is_none() {
        return run_sparse(q, text, lower);
    }
    run_dp(q, text, lower, indices_out)
}

fn run_dp(
    q: &[char],
    text: &str,
    lower: Option<&str>,
    indices_out: Option<&mut Vec<usize>>,
) -> Option<i64> {
    let m = q.len();
    if m == 0 {
        if let Some(out) = indices_out {
            out.clear();
        }
        return Some(0);
    }

    // Allocation-free rejection: lowercase on the fly and look for the query as a subsequence.
    let mut qi = 0;
    let mut n = 0usize;
    match lower {
        Some(lower) => {
            for c in lower.chars() {
                n += 1;
                if qi < m && c == q[qi] {
                    qi += 1;
                }
            }
        }
        None => {
            for c in text.chars() {
                n += 1;
                if qi < m && lc(c) == q[qi] {
                    qi += 1;
                }
            }
        }
    }
    if qi < m || m > n {
        return None;
    }

    SCRATCH.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Scratch { t, tl, dp, from } = &mut *guard;
        t.clear();
        t.extend(text.chars());
        tl.clear();
        match lower {
            Some(lower) => tl.extend(lower.chars()),
            None => tl.extend(t.iter().map(|&c| lc(c))),
        }
        debug_assert_eq!(t.len(), tl.len());
        dp.clear();
        dp.resize(m * n, NEG);
        from.clear();
        from.resize(m * n, NO_PREV);

        for i in 0..m {
            // Running maximum of dp[i-1][k] over k <= j-7 (those all share the same capped gap
            // penalty), with the smallest k winning ties, exactly like the naive scan.
            let (mut pm_val, mut pm_idx) = (NEG, NO_PREV);
            for j in i..n {
                if i > 0 {
                    // k = j-7 has just become part of the "far" prefix.
                    if j >= 7 && j - 7 >= i - 1 {
                        let k = j - 7;
                        let v = dp[(i - 1) * n + k];
                        if v != NEG && v > pm_val {
                            pm_val = v;
                            pm_idx = k as u32;
                        }
                    }
                }
                if tl[j] != q[i] {
                    continue;
                }
                let mut bonus = 8;
                if j == 0 {
                    bonus += 16;
                } else {
                    let p = t[j - 1];
                    if is_sep(p) {
                        bonus += 10;
                    } else if p.is_lowercase() && t[j].is_uppercase() {
                        bonus += 8;
                    }
                }
                if i == 0 {
                    dp[j] = bonus - (j as i64).min(10);
                } else {
                    // Far predecessors (k <= j-7) all pay the capped gap penalty: -(6) - 1.
                    let (mut best, mut bk) = if pm_val != NEG {
                        (pm_val - 7, pm_idx)
                    } else {
                        (NEG, NO_PREV)
                    };
                    // Nearby predecessors k in [j-6, j-1]: gap penalty grows with distance,
                    // an adjacent predecessor earns the consecutive-match bonus.
                    let lo = (i - 1).max(j.saturating_sub(6));
                    for k in lo..j {
                        let prev = dp[(i - 1) * n + k];
                        if prev == NEG {
                            continue;
                        }
                        let gap = if k + 1 == j {
                            12
                        } else {
                            -((j - k - 1) as i64).min(6) - 1
                        };
                        let s = prev + gap;
                        if s > best {
                            best = s;
                            bk = k as u32;
                        }
                    }
                    if best != NEG {
                        dp[i * n + j] = best + bonus;
                        from[i * n + j] = bk;
                    }
                }
            }
        }

        let last = (m - 1) * n;
        let (mut bj, mut bs) = (usize::MAX, NEG);
        for j in 0..n {
            if dp[last + j] > bs {
                bs = dp[last + j];
                bj = j;
            }
        }
        if bj == usize::MAX {
            return None;
        }

        let contiguous_prefix;
        if let Some(out) = indices_out {
            out.clear();
            out.resize(m, 0);
            let mut j = bj;
            for i in (0..m).rev() {
                out[i] = j;
                if i > 0 {
                    j = from[i * n + j] as usize;
                }
            }
            contiguous_prefix = out.iter().enumerate().all(|(k, &ix)| k == ix);
        } else {
            // Without the index vector the walk-back is still needed to know whether the match
            // is a contiguous prefix; do it without allocating.
            let mut j = bj;
            let mut all = true;
            for i in (0..m).rev() {
                if i != j {
                    all = false;
                    break;
                }
                if i > 0 {
                    j = from[i * n + j] as usize;
                }
            }
            contiguous_prefix = all;
        }

        let mut score = bs - (n as i64) / 8;
        if contiguous_prefix {
            score += 20; // contiguous prefix
            if m == n {
                score += 40; // exact
            }
        }
        Some(score)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_beats_scattered() {
        let a = fuzzy("fire", "Firefox").unwrap().score;
        let b = fuzzy("fire", "File Recovery Tool").unwrap().score;
        assert!(a > b);
    }
    #[test]
    fn camel_and_boundary() {
        assert!(fuzzy("vsc", "VisualStudioCode").is_some());
        assert!(
            fuzzy("vsc", "visual studio code").unwrap().score
                > fuzzy("vsc", "advisoryscan").unwrap().score
        );
    }
    #[test]
    fn exact_filename_beats_a_same_named_directory() {
        let query = Query::new("hosts");
        let file = file_name_score(&query, "hosts", "hosts", false).unwrap();
        let directory = file_name_score(&query, "Hosts", "hosts", true).unwrap();
        let fuzzy = file_name_score(&query, "hosts-backup", "hosts-backup", false).unwrap();
        assert!(file > directory);
        assert!(file > fuzzy);
    }

    #[test]
    fn exact_filename_stem_beats_a_fuzzy_match() {
        let query = Query::new("cargo");
        let exact_stem = file_name_score(&query, "Cargo.toml", "cargo.toml", false).unwrap();
        let scattered =
            file_name_score(&query, "c_a_r_g_o_notes", "c_a_r_g_o_notes", false).unwrap();
        assert!(exact_stem > scattered);
    }

    #[test]
    fn exact_and_prefix_bands_beat_long_boundary_heavy_fuzzy_names() {
        let query_text = "a".repeat(128);
        let query = Query::new(&query_text);
        let exact = file_name_score(&query, &query_text, &query_text, false).unwrap();

        // 255 characters: every query letter after the first follows a separator and receives
        // a word-boundary bonus. This used to score above the old 2,000-point exact bonus.
        let fuzzy_name = format!("{}a", "a_".repeat(127));
        assert_eq!(fuzzy_name.chars().count(), 255);
        let fuzzy = file_name_score(&query, &fuzzy_name, &fuzzy_name, false).unwrap();

        let prefix_name = format!("{query_text}z");
        let prefix = file_name_score(&query, &prefix_name, &prefix_name, false).unwrap();
        assert!(exact > prefix, "exact={exact}, prefix={prefix}");
        assert!(prefix > fuzzy, "prefix={prefix}, fuzzy={fuzzy}");
    }

    #[test]
    fn exact_and_prefix_filename_ranking_ignore_whitespace() {
        let exact_query = Query::new("my file");
        let exact = file_name_score(&exact_query, "My File", "my file", false).unwrap();
        let fuzzy =
            file_name_score(&exact_query, "my final report", "my final report", false).unwrap();
        assert!(exact > fuzzy);

        let prefix_query = Query::new("my f");
        assert!(file_name_score(&prefix_query, "My Folder.txt", "my folder.txt", false,).is_some());
    }

    #[test]
    fn no_match() {
        assert!(fuzzy("xyz", "firefox").is_none());
    }
    #[test]
    fn exact_wins() {
        assert!(fuzzy("code", "code").unwrap().score > fuzzy("code", "codec").unwrap().score);
    }

    /// The straightforward O(m*n^2) implementation this module replaced, kept verbatim as the
    /// oracle for the optimized version.
    mod reference {
        use super::super::{is_sep, lc, normalize};
        pub fn fuzzy(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
            const NEG: i64 = i64::MIN / 4;
            let q = normalize(query);
            let t: Vec<char> = text.chars().collect();
            let tl: Vec<char> = t.iter().map(|&c| lc(c)).collect();
            let (m, n) = (q.len(), t.len());
            if m == 0 {
                return Some((0, vec![]));
            }
            if m > n {
                return None;
            }
            let mut qi = 0;
            for &c in &tl {
                if qi < m && c == q[qi] {
                    qi += 1;
                }
            }
            if qi < m {
                return None;
            }
            let mut dp = vec![vec![NEG; n]; m];
            let mut from = vec![vec![usize::MAX; n]; m];
            for i in 0..m {
                for j in i..n {
                    if tl[j] != q[i] {
                        continue;
                    }
                    let mut bonus = 8;
                    if j == 0 {
                        bonus += 16;
                    } else {
                        let p = t[j - 1];
                        if is_sep(p) {
                            bonus += 10;
                        } else if p.is_lowercase() && t[j].is_uppercase() {
                            bonus += 8;
                        }
                    }
                    if i == 0 {
                        dp[0][j] = bonus - (j as i64).min(10);
                    } else {
                        let (mut best, mut bk) = (NEG, usize::MAX);
                        for (k, &prev) in dp[i - 1].iter().enumerate().take(j).skip(i - 1) {
                            if prev == NEG {
                                continue;
                            }
                            let gap = if k + 1 == j {
                                12
                            } else {
                                -((j - k - 1) as i64).min(6) - 1
                            };
                            let s = prev + gap;
                            if s > best {
                                best = s;
                                bk = k;
                            }
                        }
                        if best != NEG {
                            dp[i][j] = best + bonus;
                            from[i][j] = bk;
                        }
                    }
                }
            }
            let (mut bj, mut bs) = (usize::MAX, NEG);
            for (j, &v) in dp[m - 1].iter().enumerate() {
                if v > bs {
                    bs = v;
                    bj = j;
                }
            }
            if bj == usize::MAX {
                return None;
            }
            let mut indices = vec![0; m];
            let mut j = bj;
            for i in (0..m).rev() {
                indices[i] = j;
                if i > 0 {
                    j = from[i][j];
                }
            }
            let mut score = bs - (n as i64) / 8;
            if indices.iter().enumerate().all(|(k, &ix)| k == ix) {
                score += 20;
                if m == n {
                    score += 40;
                }
            }
            Some((score, indices))
        }
    }

    fn rng(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *seed >> 33
    }

    /// Random strings over a small alphabet (lots of repeated letters, separators, camelCase
    /// and non-ASCII) so ties, gaps longer than the penalty cap and boundary bonuses all occur.
    fn random_text(seed: &mut u64, max_len: usize) -> String {
        const ALPHA: &[char] = &[
            'a', 'b', 'c', 'A', 'B', 'C', ' ', '_', '-', '.', '/', 'é', 'É', 'ß', '1',
        ];
        let len = 1 + (rng(seed) as usize % max_len);
        (0..len)
            .map(|_| ALPHA[rng(seed) as usize % ALPHA.len()])
            .collect()
    }

    #[test]
    fn identical_to_reference_on_random_input() {
        let mut seed = 7u64;
        let (mut compared, mut matched) = (0u32, 0u32);
        for _ in 0..60_000 {
            let text = random_text(&mut seed, 60);
            let query = random_text(&mut seed, 6);
            let want = reference::fuzzy(&query, &text);
            let got = fuzzy(&query, &text);
            let fast = score(&Query::new(&query), &text);
            compared += 1;
            match (want, got) {
                (None, None) => {
                    assert!(fast.is_none(), "q={query:?} t={text:?}");
                    // The mask filter must never reject something that is a subsequence match.
                }
                (Some((ws, wi)), Some(g)) => {
                    matched += 1;
                    assert_eq!(ws, g.score, "score differs q={query:?} t={text:?}");
                    assert_eq!(wi, g.indices, "indices differ q={query:?} t={text:?}");
                    assert_eq!(
                        Some(ws),
                        fast,
                        "score-only path differs q={query:?} t={text:?}"
                    );
                    let lower: String = text.chars().map(lc).collect();
                    assert_eq!(
                        Some(ws),
                        score_with_lower(&Query::new(&query), &text, &lower),
                        "lower path differs q={query:?} t={text:?}"
                    );
                }
                (w, g) => panic!(
                    "match/no-match disagreement q={query:?} t={text:?} ref={w:?} new={:?}",
                    g.map(|m| m.score)
                ),
            }
        }
        assert!(
            matched > 5_000,
            "test not exercising enough matches ({matched}/{compared})"
        );
    }

    #[test]
    fn mask_never_rejects_a_real_match() {
        let mut seed = 99u64;
        for _ in 0..60_000 {
            let text = random_text(&mut seed, 40);
            let query = random_text(&mut seed, 5);
            let q = Query::new(&query);
            let lower: String = text.chars().map(lc).collect();
            if is_subsequence(q.chars(), &lower) {
                let tm = char_mask(lower.chars());
                assert_eq!(
                    q.mask() & !tm,
                    0,
                    "mask rejected a subsequence match q={query:?} t={text:?}"
                );
            }
        }
    }

    #[test]
    fn long_text_gaps_beyond_penalty_cap() {
        // Forces the far-prefix path (gap > 6) and long names.
        let text = format!("a{}b{}c", "x".repeat(40), "y".repeat(25));
        for q in ["abc", "ab", "ac", "bc"] {
            let want = reference::fuzzy(q, &text).unwrap();
            let got = fuzzy(q, &text).unwrap();
            assert_eq!((want.0, want.1), (got.score, got.indices));
        }
    }

    /// Differential test: the sparse score-only DP must return exactly what the full DP
    /// returns, on randomized names with separators, mixed case, digits, whitespace and
    /// non-ASCII letters, for both the `lower` and the on-the-fly-lowercase paths.
    #[test]
    fn sparse_score_matches_the_dp() {
        const ALPHA: &[char] = &[
            'a', 'b', 'c', 'A', 'B', 'C', '_', '-', '.', ' ', '1', '2', '/', 'é', 'É',
        ];
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut compared, mut matched) = (0usize, 0usize);
        for round in 0..600_000 {
            let len = 1 + (next() % if round % 10 == 0 { 40 } else { 16 }) as usize;
            let text: String = (0..len)
                .map(|_| ALPHA[(next() % ALPHA.len() as u64) as usize])
                .collect();
            let lower: String = text.chars().map(lc).collect();
            let q: Vec<char> = match next() % 3 {
                // true prefix, true subsequence, or arbitrary short query
                0 => {
                    let take = 1 + (next() % len as u64) as usize;
                    lower
                        .chars()
                        .take(take)
                        .filter(|c| !c.is_whitespace())
                        .collect()
                }
                1 => lower
                    .chars()
                    .filter(|_| next() % 2 == 0)
                    .filter(|c| !c.is_whitespace())
                    .collect(),
                _ => (0..1 + (next() % 4) as usize)
                    .map(|_| lc(ALPHA[(next() % ALPHA.len() as u64) as usize]))
                    .filter(|c| !c.is_whitespace())
                    .collect(),
            };
            if q.is_empty() {
                continue;
            }
            for lower_arg in [Some(lower.as_str()), None] {
                let dp = run_dp(&q, &text, lower_arg, None);
                let sparse = run_sparse(&q, &text, lower_arg);
                assert_eq!(
                    dp,
                    sparse,
                    "text {text:?} query {q:?} lower={}",
                    lower_arg.is_some()
                );
                compared += 1;
                matched += dp.is_some() as usize;
            }
        }
        assert!(
            matched > 200_000,
            "too few matching cases: {matched}/{compared}"
        );
    }

    /// The non-prefix ceiling must hold for every non-prefix candidate (it is what lets the
    /// search skip them once the heap is full of prefix-band hits).
    #[test]
    fn non_prefix_ceiling_bounds_every_non_prefix_score() {
        const ALPHA: &[char] = &['a', 'b', 'c', 'A', 'B', '_', '-', '.', ' ', '1', '/'];
        let mut seed = 0xDEAD_BEEF_CAFE_F00Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..300_000 {
            let len = 1 + (next() % 20) as usize;
            let name: String = (0..len)
                .map(|_| ALPHA[(next() % ALPHA.len() as u64) as usize])
                .collect();
            let lower: String = name.chars().map(lc).collect();
            let qlen = 1 + (next() % 5) as usize;
            let text_q: String = (0..qlen)
                .map(|_| ALPHA[(next() % ALPHA.len() as u64) as usize])
                .collect();
            let query = Query::new(&text_q);
            if query.is_empty() || is_normalized_prefix(&lower, &query) {
                continue;
            }
            for is_dir in [false, true] {
                if let Some(score) = file_name_score(&query, &name, &lower, is_dir) {
                    assert!(
                        score <= non_prefix_score_ceiling(query.len()),
                        "name {name:?} query {text_q:?}: {score} > {}",
                        non_prefix_score_ceiling(query.len())
                    );
                }
            }
        }
    }
}
