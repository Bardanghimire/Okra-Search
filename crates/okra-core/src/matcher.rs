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
    mask: u64,
}

impl Query {
    pub fn new(query: &str) -> Self {
        let chars = normalize(query);
        let mask = char_mask(chars.iter().copied());
        Self { chars, mask }
    }
    /// See [`char_mask`]: `entry_mask & query.mask() == query.mask()` is necessary for a match.
    pub fn mask(&self) -> u64 {
        self.mask
    }
    pub fn chars(&self) -> &[char] {
        &self.chars
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

fn run(
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
}
