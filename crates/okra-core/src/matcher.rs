//! Fuzzy matcher: best-alignment subsequence scoring (DP) with bonuses for
//! prefix, word-boundary, camelCase and consecutive matches.

pub struct Match {
    pub score: i64,
    pub indices: Vec<usize>,
}

#[inline]
pub fn lc(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
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

pub fn fuzzy(query: &str, text: &str) -> Option<Match> {
    const NEG: i64 = i64::MIN / 4;
    let q = normalize(query);
    let t: Vec<char> = text.chars().collect();
    let tl: Vec<char> = t.iter().map(|&c| lc(c)).collect();
    let (m, n) = (q.len(), t.len());
    if m == 0 {
        return Some(Match { score: 0, indices: vec![] });
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
                for k in (i - 1)..j {
                    if dp[i - 1][k] == NEG {
                        continue;
                    }
                    let gap = if k + 1 == j { 12 } else { -((j - k - 1) as i64).min(6) - 1 };
                    let s = dp[i - 1][k] + gap;
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
    for j in 0..n {
        if dp[m - 1][j] > bs {
            bs = dp[m - 1][j];
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
        score += 20; // contiguous prefix
        if m == n {
            score += 40; // exact
        }
    }
    Some(Match { score, indices })
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
        assert!(fuzzy("vsc", "visual studio code").unwrap().score > fuzzy("vsc", "advisoryscan").unwrap().score);
    }
    #[test]
    fn no_match() {
        assert!(fuzzy("xyz", "firefox").is_none());
    }
    #[test]
    fn exact_wins() {
        assert!(fuzzy("code", "code").unwrap().score > fuzzy("code", "codec").unwrap().score);
    }
}
