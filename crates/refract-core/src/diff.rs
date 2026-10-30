//! A small unified line diff, for showing what installing the status hook changes.

/// Lines of context around each change.
const CONTEXT: usize = 3;

/// The most cells the longest-common-subsequence table may have; past it the changed
/// middle is shown as all old lines removed and all new lines added.
const MAX_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Same,
    Del,
    Add,
}

/// A unified diff from `old` to `new`, with `name` in both file headers. Empty when the
/// texts are equal. Lines are compared whole; a last line without a newline is marked
/// `\ No newline at end of file`, as `diff -u` does.
pub fn unified_diff(name: &str, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let ops = line_ops(&a, &b);

    let mut out = format!("--- {name}\n+++ {name}\n");
    // Each op with the old and new line indexes it starts at.
    let mut at = Vec::with_capacity(ops.len());
    let (mut i, mut j) = (0, 0);
    for &op in &ops {
        at.push((op, i, j));
        match op {
            Op::Same => (i, j) = (i + 1, j + 1),
            Op::Del => i += 1,
            Op::Add => j += 1,
        }
    }
    let changed: Vec<usize> = (0..at.len()).filter(|&k| at[k].0 != Op::Same).collect();
    let mut k = 0;
    while k < changed.len() {
        // Grow the hunk while the next change is close enough to share context.
        let start = changed[k].saturating_sub(CONTEXT);
        let mut end = changed[k];
        while k + 1 < changed.len() && changed[k + 1] - end <= 2 * CONTEXT {
            k += 1;
            end = changed[k];
        }
        let end = (end + CONTEXT + 1).min(at.len());
        k += 1;

        let hunk = &at[start..end];
        let old_len = hunk.iter().filter(|h| h.0 != Op::Add).count();
        let new_len = hunk.iter().filter(|h| h.0 != Op::Del).count();
        let (_, i0, j0) = hunk[0];
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(i0, old_len),
            range(j0, new_len)
        ));
        for &(op, i, j) in hunk {
            let (mark, line) = match op {
                Op::Same => (' ', a[i]),
                Op::Del => ('-', a[i]),
                Op::Add => ('+', b[j]),
            };
            out.push(mark);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    out
}

/// `start,len` as hunk headers write it: one-based, `,1` left out, and an empty range
/// named by the line before it.
fn range(start: usize, len: usize) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}

/// The edit script from `a` to `b`: a longest common subsequence over the lines
/// between the common prefix and suffix.
fn line_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let (n, m) = (am.len(), bm.len());
    let mut ops = vec![Op::Same; prefix];
    if n.saturating_mul(m) > MAX_CELLS {
        ops.extend(std::iter::repeat_n(Op::Del, n));
        ops.extend(std::iter::repeat_n(Op::Add, m));
        ops.extend(std::iter::repeat_n(Op::Same, suffix));
        return ops;
    }
    // lcs[i][j]: longest common subsequence of am[i..] and bm[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if am[i] == bm[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && am[i] == bm[j] {
            ops.push(Op::Same);
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push(Op::Del);
            i += 1;
        } else {
            ops.push(Op::Add);
            j += 1;
        }
    }
    ops.extend(std::iter::repeat_n(Op::Same, suffix));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_texts_give_nothing() {
        assert_eq!(unified_diff("s.json", "a\nb\n", "a\nb\n"), "");
    }

    #[test]
    fn an_insertion_gets_context_and_a_header() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let new = "1\n2\n3\n4\nnew\n5\n6\n7\n8\n";
        assert_eq!(
            unified_diff("s.json", old, new),
            "--- s.json\n+++ s.json\n@@ -2,6 +2,7 @@\n 2\n 3\n 4\n+new\n 5\n 6\n 7\n"
        );
    }

    #[test]
    fn far_apart_changes_make_two_hunks_and_near_ones_one() {
        let lines = |swap: &[(u32, &str)]| -> String {
            (1..=20)
                .map(|n| match swap.iter().find(|s| s.0 == n) {
                    Some((_, word)) => format!("{word}\n"),
                    None => format!("{n}\n"),
                })
                .collect()
        };
        let old = lines(&[]);
        let diff = unified_diff("f", &old, &lines(&[(2, "two"), (19, "nineteen")]));
        assert_eq!(diff.matches("@@ -").count(), 2);
        assert!(diff.contains("@@ -1,5 +1,5 @@\n 1\n-2\n+two\n 3\n"));
        assert!(diff.contains("@@ -16,5 +16,5 @@\n"));
        let near = lines(&[(5, "five"), (11, "eleven")]);
        assert_eq!(unified_diff("f", &old, &near).matches("@@ -").count(), 1);
    }

    #[test]
    fn whole_file_added_or_removed() {
        assert_eq!(
            unified_diff("f", "", "{}\n"),
            "--- f\n+++ f\n@@ -0,0 +1 @@\n+{}\n"
        );
        assert_eq!(
            unified_diff("f", "{}\n", ""),
            "--- f\n+++ f\n@@ -1 +0,0 @@\n-{}\n"
        );
    }

    #[test]
    fn huge_changes_skip_the_table() {
        let old: String = (0..2100).map(|n| format!("a{n}\n")).collect();
        let new: String = (0..2100).map(|n| format!("b{n}\n")).collect();
        let diff = unified_diff("f", &format!("x\n{old}"), &format!("x\n{new}"));
        assert!(diff.starts_with("--- f\n+++ f\n@@ -1,2101 +1,2101 @@\n x\n-a0\n"));
        assert_eq!(diff.matches("\n-a").count(), 2100);
        assert_eq!(diff.matches("\n+b").count(), 2100);
    }

    #[test]
    fn a_missing_final_newline_is_marked() {
        assert_eq!(
            unified_diff("f", "a\nb", "a\nb\n"),
            "--- f\n+++ f\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n"
        );
    }
}
