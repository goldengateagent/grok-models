//! Port of Python's `difflib` pieces used by grok-models.py:
//! `SequenceMatcher.ratio()` and `get_close_matches(word, possibilities)`
//! (default n=3, cutoff=0.6), so "did you mean" hints match exactly.

use std::collections::HashMap;

#[derive(Clone, Copy)]
struct Block {
    i: usize,
    j: usize,
    n: usize,
}

/// SequenceMatcher with a fixed seq2 and swappable seq1 (autojunk disabled —
/// ids are far below the 200-char autojunk threshold).
pub struct SequenceMatcher<'b> {
    b: Vec<char>,
    b2j: HashMap<char, Vec<usize>>,
    _marker: std::marker::PhantomData<&'b [char]>,
}

impl<'b> SequenceMatcher<'b> {
    pub fn new(seq2: &str) -> Self {
        let b: Vec<char> = seq2.chars().collect();
        let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
        for (j, &ch) in b.iter().enumerate() {
            b2j.entry(ch).or_default().push(j);
        }
        SequenceMatcher {
            b,
            b2j,
            _marker: std::marker::PhantomData,
        }
    }

    fn find_longest_match(&self, a: &[char], alo: usize, ahi: usize, blo: usize, bhi: usize) -> Block {
        // j2len[j] = length of longest match ending at a[i-1], b[j-1]
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        let mut best_i = alo;
        let mut best_j = blo;
        let mut best_size = 0usize;
        for i in alo..ahi {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(indices) = self.b2j.get(&a[i]) {
                for &j in indices {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j2len.get(&(j.wrapping_sub(1))).copied().unwrap_or(0) + 1;
                    newj2len.insert(j, k);
                    if k > best_size {
                        best_i = i + 1 - k;
                        best_j = j + 1 - k;
                        best_size = k;
                    }
                }
            }
            j2len = newj2len;
        }
        // Extend over adjacent equal elements (no junk handling needed).
        while best_i > alo && best_j > blo && a[best_i - 1] == self.b[best_j - 1] {
            best_i -= 1;
            best_j -= 1;
            best_size += 1;
        }
        while best_i + best_size < ahi
            && best_j + best_size < bhi
            && a[best_i + best_size] == self.b[best_j + best_size]
        {
            best_size += 1;
        }
        Block { i: best_i, j: best_j, n: best_size }
    }

    /// `get_matching_blocks()` — queue-based recursion, sorted and merged.
    fn matching_blocks(&self, a: &[char]) -> Vec<Block> {
        let la = a.len();
        let lb = self.b.len();
        let mut blocks: Vec<Block> = Vec::new();
        let mut queue: Vec<(usize, usize, usize, usize)> = vec![(0, la, 0, lb)];
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let m = self.find_longest_match(a, alo, ahi, blo, bhi);
            if m.n > 0 {
                if alo < m.i && blo < m.j {
                    queue.push((alo, m.i, blo, m.j));
                }
                if m.i + m.n < ahi && m.j + m.n < bhi {
                    queue.push((m.i + m.n, ahi, m.j + m.n, bhi));
                }
                blocks.push(m);
            }
        }
        blocks.sort_by_key(|b| (b.i, b.j));
        // Merge adjacent blocks.
        let mut merged: Vec<Block> = Vec::new();
        for b in blocks {
            if let Some(last) = merged.last_mut() {
                if last.i + last.n == b.i && last.j + last.n == b.j {
                    last.n += b.n;
                    continue;
                }
            }
            merged.push(b);
        }
        merged.push(Block { i: la, j: lb, n: 0 });
        merged
    }

    pub fn ratio(&self, a: &str) -> f64 {
        let av: Vec<char> = a.chars().collect();
        let matches: usize = self.matching_blocks(&av).iter().map(|b| b.n).sum();
        let denom = av.len() + self.b.len();
        if denom == 0 {
            return 1.0;
        }
        2.0 * matches as f64 / denom as f64
    }

    /// Upper bound via character multiset intersection.
    pub fn quick_ratio(&self, a: &str) -> f64 {
        let av: Vec<char> = a.chars().collect();
        let mut avail: HashMap<char, isize> = self.b2j.iter().map(|(k, v)| (*k, v.len() as isize)).collect();
        let mut sums = 0isize;
        for &ch in &av {
            let e = avail.entry(ch).or_insert(0);
            if *e > 0 {
                *e -= 1;
                sums += 1;
            }
        }
        let denom = av.len() + self.b.len();
        if denom == 0 {
            return 1.0;
        }
        2.0 * sums as f64 / denom as f64
    }

    pub fn real_quick_ratio(&self, a: &str) -> f64 {
        let la = a.chars().count();
        let lb = self.b.len();
        let denom = la + lb;
        if denom == 0 {
            return 1.0;
        }
        2.0 * la.min(lb) as f64 / denom as f64
    }
}

/// Python `difflib.get_close_matches(word, possibilities)` (n=3, cutoff=0.6).
pub fn get_close_matches(word: &str, possibilities: &[String]) -> Vec<String> {
    get_close_matches_n(word, possibilities, 3, 0.6)
}

pub fn get_close_matches_n(word: &str, possibilities: &[String], n: usize, cutoff: f64) -> Vec<String> {
    let sm = SequenceMatcher::new(word);
    let mut scored: Vec<(f64, String)> = Vec::new();
    for x in possibilities {
        if sm.real_quick_ratio(x) >= cutoff && sm.quick_ratio(x) >= cutoff && sm.ratio(x) >= cutoff {
            scored.push((sm.ratio(x), x.clone()));
        }
    }
    // heapq.nlargest: score descending, ties keep earlier entries first.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(n);
    scored.into_iter().map(|(_, x)| x).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_matches_basic() {
        let words: Vec<String> = ["opencode", "openrouter", "anthropic", "openai"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            get_close_matches("opencod", &words),
            vec!["opencode".to_string(), "openai".to_string()]
        );
        assert_eq!(get_close_matches("zzz", &words), Vec::<String>::new());
    }

    #[test]
    fn ratio_matches_python() {
        // Values verified against CPython difflib.
        let sm = SequenceMatcher::new("abcdef");
        assert!((sm.ratio("abcd") - 8.0 / 10.0).abs() < 1e-12);
        assert!((sm.ratio("abcdef") - 1.0).abs() < 1e-12);
    }
}
