//! Shell line colors for the config info block. A copy of the tokenizer in
//! `tui.rs`. This module does not call that file.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodeColor {
    Text,
    Comment,
    Error,
    String,
    Symbol,
    Var,
}

/// Syntax-color one code line: gold symbols (`=`, quotes, redirections), white
/// strings and plain text, cyan comments, green variable names. `highlight`
/// optionally recolors a char span `(start, end, color)`. An empty `VAR = ""`
/// assignment is auto-flagged red.
pub(crate) fn code_line_segments(
    ln: &str,
    highlight: Option<(usize, usize, CodeColor)>,
) -> Vec<(String, CodeColor)> {
    let chars: Vec<char> = ln.chars().collect();
    let n = chars.len();
    if chars.iter().all(|c| c.is_whitespace()) || ln.trim_start().starts_with('#') {
        return vec![(ln.to_string(), CodeColor::Comment)];
    }
    let mut attrs = vec![CodeColor::Text; n];

    let assign_end = sh_assign_name_end(&chars);
    if let Some(name_end) = assign_end {
        for a in attrs.iter_mut().take(name_end) {
            *a = CodeColor::Var;
        }
    } else if n > 0 {
        let start = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(0);
        let end = chars[start..]
            .iter()
            .position(|c| c.is_whitespace())
            .map(|p| start + p)
            .unwrap_or(n);
        for a in attrs[start..end].iter_mut() {
            *a = CodeColor::Symbol;
        }
    }

    let mut in_single = false;
    let mut in_double = false;
    let mut sq_open: Option<usize> = None;
    let mut dq_open: Option<usize> = None;
    for i in 0..n {
        let ch = chars[i];
        if ch == '\'' && !in_double {
            attrs[i] = CodeColor::Symbol;
            if in_single {
                for a in attrs[sq_open.unwrap_or(0) + 1..i].iter_mut() {
                    *a = CodeColor::String;
                }
                in_single = false;
                sq_open = None;
            } else {
                in_single = true;
                sq_open = Some(i);
            }
        } else if ch == '"' && !in_single {
            attrs[i] = CodeColor::Symbol;
            if in_double {
                for a in attrs[dq_open.unwrap_or(0) + 1..i].iter_mut() {
                    *a = CodeColor::String;
                }
                in_double = false;
                dq_open = None;
            } else {
                in_double = true;
                dq_open = Some(i);
            }
        } else if "=<>|;".contains(ch) && !in_single && !in_double {
            attrs[i] = CodeColor::Symbol;
        }
    }

    let highlight = highlight.or_else(|| {
        let name_end = sh_assign_name_end(&chars)?;
        let mut i = name_end;
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i < n && chars[i] == '=' {
            i += 1;
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
            if i + 1 < n && chars[i] == '"' && chars[i + 1] == '"' {
                return Some((0, name_end, CodeColor::Error));
            }
        }
        None
    });

    if in_single {
        for a in attrs[sq_open.unwrap_or(0) + 1..n].iter_mut() {
            *a = CodeColor::String;
        }
    } else if in_double {
        for a in attrs[dq_open.unwrap_or(0) + 1..n].iter_mut() {
            *a = CodeColor::String;
        }
    }

    if let Some((hs, he, hcolor)) = highlight {
        for a in attrs[hs.min(n)..he.min(n)].iter_mut() {
            if matches!(*a, CodeColor::Var | CodeColor::Text | CodeColor::String) {
                *a = hcolor;
            }
        }
    }

    let mut runs: Vec<(usize, usize, CodeColor)> = Vec::new();
    for (i, a) in attrs.iter().enumerate() {
        match runs.last_mut() {
            Some(last) if last.2 == *a && last.1 == i => last.1 = i + 1,
            _ => runs.push((i, i + 1, *a)),
        }
    }
    runs.into_iter()
        .map(|(s, e, a)| (chars[s..e].iter().collect::<String>(), a))
        .collect()
}

/// Match `^(\S+)(\s*=)`. Returns the end of the variable-name span.
fn sh_assign_name_end(chars: &[char]) -> Option<usize> {
    let n = chars.len();
    if n == 0 || chars[0].is_whitespace() {
        return None;
    }
    let run_end = chars.iter().position(|c| c.is_whitespace()).unwrap_or(n);
    let mut j = run_end;
    while j < n && chars[j].is_whitespace() {
        j += 1;
    }
    if j < n && chars[j] == '=' {
        return Some(run_end);
    }
    if run_end == n {
        if let Some(eq) = chars.iter().position(|&c| c == '=') {
            if eq > 0 {
                return Some(eq);
            }
        }
    }
    None
}
