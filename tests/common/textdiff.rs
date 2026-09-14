//! Readable line diffs for test failure messages.

/// Renders a unified-style line diff of `expected` against `actual`.
///
/// Lines only in `expected` are prefixed `-`, lines only in `actual` `+`,
/// and common lines two spaces. A missing trailing newline is called out,
/// since that is an easy mistake in `expect.stdout`.
pub fn diff(expected: &str, actual: &str) -> String {
    let a: Vec<&str> = expected.split_inclusive('\n').collect();
    let b: Vec<&str> = actual.split_inclusive('\n').collect();
    let mut out = String::from("--- expected\n+++ actual\n");

    // Longest common subsequence; outputs are small. Fall back to printing
    // both sides when they are too large for the quadratic table.
    if a.len().saturating_mul(b.len()) > 4_000_000 {
        for line in &a {
            push_line(&mut out, '-', line);
        }
        for line in &b {
            push_line(&mut out, '+', line);
        }
        return out;
    }
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            push_line(&mut out, ' ', a[i]);
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            push_line(&mut out, '-', a[i]);
            i += 1;
        } else {
            push_line(&mut out, '+', b[j]);
            j += 1;
        }
    }
    out
}

fn push_line(out: &mut String, marker: char, line: &str) {
    out.push(marker);
    out.push(' ');
    match line.strip_suffix('\n') {
        Some(body) => {
            out.push_str(body);
            out.push('\n');
        }
        None => {
            out.push_str(line);
            out.push_str("\n\\ No newline at end\n");
        }
    }
}
