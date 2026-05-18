//! Word error rate via Levenshtein edit distance.

#[derive(Debug, Clone, Copy)]
pub struct WerReport {
    pub edits: usize,
    pub substitutions: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub reference_words: usize,
    pub hypothesis_words: usize,
    pub wer: f64,
}

pub fn wer(reference: &str, hypothesis: &str) -> WerReport {
    let r = tokenize(reference);
    let h = tokenize(hypothesis);
    let (edits, subs, ins, dels) = levenshtein_with_breakdown(&r, &h);
    let wer = if r.is_empty() {
        if h.is_empty() {
            0.0
        } else {
            1.0
        }
    } else {
        edits as f64 / r.len() as f64
    };
    WerReport {
        edits,
        substitutions: subs,
        insertions: ins,
        deletions: dels,
        reference_words: r.len(),
        hypothesis_words: h.len(),
        wer,
    }
}

fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '\'' || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn levenshtein_with_breakdown(r: &[String], h: &[String]) -> (usize, usize, usize, usize) {
    let n = r.len();
    let m = h.len();
    if n == 0 {
        return (m, 0, m, 0);
    }
    if m == 0 {
        return (n, 0, 0, n);
    }

    // Op type per cell so we can reconstruct the path for the s/i/d split.
    // 0 = match (no edit), 1 = substitution, 2 = insertion (H), 3 = deletion (R).
    let mut cost = vec![vec![0usize; m + 1]; n + 1];
    let mut op = vec![vec![0u8; m + 1]; n + 1];
    for i in 0..=n {
        cost[i][0] = i;
        op[i][0] = 3;
    }
    for j in 0..=m {
        cost[0][j] = j;
        op[0][j] = 2;
    }
    op[0][0] = 0;

    for i in 1..=n {
        for j in 1..=m {
            if r[i - 1] == h[j - 1] {
                cost[i][j] = cost[i - 1][j - 1];
                op[i][j] = 0;
            } else {
                let sub = cost[i - 1][j - 1] + 1;
                let ins = cost[i][j - 1] + 1;
                let del = cost[i - 1][j] + 1;
                let m_cost = sub.min(ins).min(del);
                cost[i][j] = m_cost;
                op[i][j] = if m_cost == sub {
                    1
                } else if m_cost == ins {
                    2
                } else {
                    3
                };
            }
        }
    }

    let mut i = n;
    let mut j = m;
    let mut subs = 0;
    let mut ins = 0;
    let mut dels = 0;
    while i > 0 || j > 0 {
        match op[i][j] {
            0 => {
                i -= 1;
                j -= 1;
            }
            1 => {
                subs += 1;
                i -= 1;
                j -= 1;
            }
            2 => {
                ins += 1;
                j -= 1;
            }
            3 => {
                dels += 1;
                i -= 1;
            }
            _ => unreachable!(),
        }
    }
    (subs + ins + dels, subs, ins, dels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_strings_have_zero_wer() {
        let r = wer("hello world", "hello world");
        assert_eq!(r.edits, 0);
        assert_eq!(r.wer, 0.0);
    }

    #[test]
    fn one_substitution() {
        let r = wer("the quick brown fox", "the quick brown dog");
        assert_eq!(r.substitutions, 1);
        assert_eq!(r.insertions, 0);
        assert_eq!(r.deletions, 0);
        assert!((r.wer - 0.25).abs() < 1e-9);
    }

    #[test]
    fn one_insertion() {
        let r = wer("the quick brown fox", "the very quick brown fox");
        assert_eq!(r.insertions, 1);
        assert_eq!(r.substitutions, 0);
        assert_eq!(r.deletions, 0);
    }

    #[test]
    fn one_deletion() {
        let r = wer("the quick brown fox", "the brown fox");
        assert_eq!(r.deletions, 1);
        assert_eq!(r.substitutions, 0);
        assert_eq!(r.insertions, 0);
    }

    #[test]
    fn punctuation_and_case_are_normalised() {
        let r = wer("Hello, world!", "hello world");
        assert_eq!(r.edits, 0);
    }

    #[test]
    fn empty_reference_with_hypothesis_words() {
        let r = wer("", "hello world");
        assert_eq!(r.edits, 2);
        assert_eq!(r.insertions, 2);
        assert_eq!(r.wer, 1.0);
    }

    #[test]
    fn empty_both_yields_zero() {
        let r = wer("", "");
        assert_eq!(r.edits, 0);
        assert_eq!(r.wer, 0.0);
    }
}
