//! Line-local lexical highlighters for Rust and Markdown (spec §16:
//! "viewport lexical only"). Pure functions over one line of bytes, no
//! allocation beyond the caller's `out` vec, no cross-line state.
//!
//! ponytail: everything here is line-local by design. Constructs that span
//! lines (block comments, fenced code bodies, unterminated strings) are
//! truncated to the current line. Real cross-line state (open `/* */`,
//! inside-a-fence tracking) is deferred to the out-of-process plugin that
//! wraps these fns later — that plugin owns a small state machine fed the
//! previous line's exit state; this module never will.

#![allow(dead_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tok {
    Keyword,
    Ident,
    Number,
    Str,
    Comment,
    Punct,
    Heading,
    Emphasis,
    CodeSpan,
    Link,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub tok: Tok,
}

// ---------- Rust ----------

// Full 2021-edition strict keyword list (2018 additions async/await/dyn
// included; `try` is reserved-but-unused so it's left out — it's not a
// keyword yet).
const RUST_KEYWORDS: &[&[u8]] = &[
    b"as", b"break", b"const", b"continue", b"crate", b"dyn", b"else", b"enum", b"extern",
    b"false", b"fn", b"for", b"if", b"impl", b"in", b"let", b"loop", b"match", b"mod", b"move",
    b"mut", b"pub", b"ref", b"return", b"self", b"Self", b"static", b"struct", b"super", b"trait",
    b"true", b"type", b"unsafe", b"use", b"where", b"while", b"async", b"await",
];

fn is_rust_keyword(word: &[u8]) -> bool {
    RUST_KEYWORDS.iter().any(|k| *k == word)
}

fn utf8_len(b: u8) -> usize {
    if b & 0x80 == 0 {
        1
    } else if b & 0xE0 == 0xC0 {
        2
    } else if b & 0xF0 == 0xE0 {
        3
    } else if b & 0xF8 == 0xF0 {
        4
    } else {
        1
    }
}

/// Scan a `"`-delimited string starting exactly at `line[q]` (`==b'"'`).
/// Returns the exclusive end index; if unterminated, that's `line.len()`.
fn scan_dquote(line: &[u8], q: usize) -> usize {
    let n = line.len();
    let mut j = q + 1;
    while j < n {
        match line[j] {
            b'\\' => j = (j + 2).min(n),
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    n
}

/// Scan a `'`-delimited char/byte-char literal starting at `line[q]`
/// (`==b'\''`), including a possible backslash escape. Returns the
/// exclusive end index whether or not a closing quote was found.
fn scan_char_lit(line: &[u8], q: usize) -> usize {
    let n = line.len();
    let mut j = q + 1;
    if j < n && line[j] == b'\\' {
        j += 1;
        if j < n {
            match line[j] {
                b'x' => {
                    j += 1;
                    for _ in 0..2 {
                        if j < n && line[j].is_ascii_hexdigit() {
                            j += 1;
                        }
                    }
                }
                b'u' => {
                    j += 1;
                    if j < n && line[j] == b'{' {
                        j += 1;
                        while j < n && line[j] != b'}' {
                            j += 1;
                        }
                        if j < n {
                            j += 1;
                        }
                    }
                }
                _ => j += 1,
            }
        }
    } else if j < n {
        j += utf8_len(line[j]);
    }
    if j < n && line[j] == b'\'' {
        j += 1;
    }
    j
}

/// `r"..."` / `r#"..."#` raw string body, `j` positioned right after the
/// `r`/`br` prefix. Returns the exclusive end if it really is a raw
/// string (zero-or-more `#` then `"`), else `None` (caller falls back to
/// treating the prefix as a plain identifier).
fn try_raw_string(line: &[u8], j0: usize) -> Option<usize> {
    let n = line.len();
    let mut j = j0;
    while j < n && line[j] == b'#' {
        j += 1;
    }
    let hashes = j - j0;
    if j >= n || line[j] != b'"' {
        return None;
    }
    j += 1;
    loop {
        if j >= n {
            return Some(n); // unterminated -> to EOL
        }
        if line[j] == b'"' {
            let mut k = j + 1;
            let mut h = 0;
            while k < n && h < hashes && line[k] == b'#' {
                k += 1;
                h += 1;
            }
            if h == hashes {
                return Some(k);
            }
        }
        j += 1;
    }
}

pub fn lex_rust_line(line: &[u8], out: &mut Vec<Span>) {
    let n = line.len();
    let mut i = 0;
    while i < n {
        let b = line[i];
        match b {
            b'/' if i + 1 < n && line[i + 1] == b'/' => {
                out.push(Span { start: i, end: n, tok: Tok::Comment });
                i = n;
            }
            // ponytail: no nesting tracked, first "*/" wins; unterminated
            // block comments run to EOL (this is the line-local ceiling).
            b'/' if i + 1 < n && line[i + 1] == b'*' => {
                let start = i;
                let mut j = i + 2;
                while j + 1 < n && !(line[j] == b'*' && line[j + 1] == b'/') {
                    j += 1;
                }
                let end = if j + 1 < n { j + 2 } else { n };
                out.push(Span { start, end, tok: Tok::Comment });
                i = end;
            }
            b'"' => {
                let start = i;
                let end = scan_dquote(line, i);
                out.push(Span { start, end, tok: Tok::Str });
                i = end;
            }
            b'\'' => {
                let start = i;
                if i + 1 < n && line[i + 1] == b'\\' {
                    i = scan_char_lit(line, i);
                    out.push(Span { start, end: i, tok: Tok::Str });
                } else {
                    let id_start = i + 1;
                    let mut j = id_start;
                    while j < n && (line[j].is_ascii_alphanumeric() || line[j] == b'_') {
                        j += 1;
                    }
                    let id_len = j - id_start;
                    if id_len == 1 && j < n && line[j] == b'\'' {
                        i = j + 1;
                        out.push(Span { start, end: i, tok: Tok::Str });
                    } else if id_len >= 1 {
                        i = j; // lifetime / loop label
                        out.push(Span { start, end: i, tok: Tok::Ident });
                    } else if j < n && line[j] >= 0x80 {
                        // non-ASCII char literal, e.g. 'é' — best effort,
                        // never split the multi-byte char across spans.
                        let w = utf8_len(line[j]);
                        let close = j + w;
                        if close < n && line[close] == b'\'' {
                            i = close + 1;
                            out.push(Span { start, end: i, tok: Tok::Str });
                        } else {
                            i += 1;
                            out.push(Span { start, end: i, tok: Tok::Punct });
                        }
                    } else {
                        i += 1;
                        out.push(Span { start, end: i, tok: Tok::Punct });
                    }
                }
            }
            b'0'..=b'9' => {
                let start = i;
                if b == b'0' && i + 1 < n && (line[i + 1] | 0x20) == b'x' {
                    i += 2;
                    while i < n && (line[i].is_ascii_hexdigit() || line[i] == b'_') {
                        i += 1;
                    }
                } else if b == b'0' && i + 1 < n && (line[i + 1] | 0x20) == b'o' {
                    i += 2;
                    while i < n && (matches!(line[i], b'0'..=b'7') || line[i] == b'_') {
                        i += 1;
                    }
                } else if b == b'0' && i + 1 < n && (line[i + 1] | 0x20) == b'b' {
                    i += 2;
                    while i < n && (line[i] == b'0' || line[i] == b'1' || line[i] == b'_') {
                        i += 1;
                    }
                } else {
                    while i < n && (line[i].is_ascii_digit() || line[i] == b'_') {
                        i += 1;
                    }
                    if i < n && line[i] == b'.' && i + 1 < n && line[i + 1].is_ascii_digit() {
                        i += 1;
                        while i < n && (line[i].is_ascii_digit() || line[i] == b'_') {
                            i += 1;
                        }
                    }
                    if i < n && (line[i] | 0x20) == b'e' {
                        let mut j = i + 1;
                        if j < n && (line[j] == b'+' || line[j] == b'-') {
                            j += 1;
                        }
                        if j < n && line[j].is_ascii_digit() {
                            i = j;
                            while i < n && (line[i].is_ascii_digit() || line[i] == b'_') {
                                i += 1;
                            }
                        }
                    }
                }
                // trailing type suffix: u32, i64, f32, usize, ...
                while i < n && (line[i].is_ascii_alphanumeric() || line[i] == b'_') {
                    i += 1;
                }
                out.push(Span { start, end: i, tok: Tok::Number });
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = i;
                while i < n && (line[i].is_ascii_alphanumeric() || line[i] == b'_') {
                    i += 1;
                }
                let word = &line[start..i];
                if (word == b"r" || word == b"br") && i < n && (line[i] == b'"' || line[i] == b'#') {
                    if let Some(end) = try_raw_string(line, i) {
                        out.push(Span { start, end, tok: Tok::Str });
                        i = end;
                        continue;
                    }
                }
                if word == b"b" && i < n && line[i] == b'"' {
                    i = scan_dquote(line, i);
                    out.push(Span { start, end: i, tok: Tok::Str });
                    continue;
                }
                if word == b"b" && i < n && line[i] == b'\'' {
                    i = scan_char_lit(line, i);
                    out.push(Span { start, end: i, tok: Tok::Str });
                    continue;
                }
                let tok = if is_rust_keyword(word) { Tok::Keyword } else { Tok::Ident };
                out.push(Span { start, end: i, tok });
            }
            _ if b < 0x80 && !b.is_ascii_whitespace() => {
                let start = i;
                i += 1;
                while i < n {
                    let c = line[i];
                    if c < 0x80
                        && !c.is_ascii_alphanumeric()
                        && c != b'_'
                        && !c.is_ascii_whitespace()
                        && c != b'"'
                        && c != b'\''
                        && c != b'/'
                    {
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(Span { start, end: i, tok: Tok::Punct });
            }
            _ => {
                // ASCII whitespace or a non-ASCII byte (unicode identifiers,
                // invalid UTF-8) — left uncovered as Text. ponytail: no
                // unicode-XID identifier support, ceiling noted.
                i += 1;
            }
        }
    }
}

// ---------- Markdown ----------

fn find_byte(line: &[u8], start: usize, target: u8) -> Option<usize> {
    let mut i = start;
    while i < line.len() {
        if line[i] == target {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// First index `>= start` where `count` consecutive `byte`s occur.
fn find_run(line: &[u8], start: usize, byte: u8, count: usize) -> Option<usize> {
    let n = line.len();
    if count == 0 || start + count > n {
        return None;
    }
    let mut p = start;
    while p + count <= n {
        if line[p..p + count].iter().all(|&c| c == byte) {
            return Some(p);
        }
        p += 1;
    }
    None
}

pub fn lex_markdown_line(line: &[u8], out: &mut Vec<Span>) {
    let n = line.len();
    if n == 0 {
        return;
    }

    // ATX heading: up to 3 leading spaces, 1-6 '#', then space or EOL.
    // ponytail: whole line marked Heading, no nested inline parsing inside
    // headings (a heading with `code` or *emph* just paints as Heading).
    let mut lead = 0;
    while lead < n && lead < 3 && line[lead] == b' ' {
        lead += 1;
    }
    let mut h = lead;
    while h < n && h - lead < 6 && line[h] == b'#' {
        h += 1;
    }
    if h > lead && (h == n || line[h] == b' ') {
        out.push(Span { start: 0, end: n, tok: Tok::Heading });
        return;
    }

    // Fenced code line: up to 3 leading spaces then >=3 backticks. Only the
    // fence line itself is tagged — content between fences needs cross-line
    // state (plugin's job later).
    let mut f = lead;
    while f < n && line[f] == b'`' {
        f += 1;
    }
    if f - lead >= 3 {
        out.push(Span { start: 0, end: n, tok: Tok::CodeSpan });
        return;
    }

    let mut i = 0;
    while i < n {
        match line[i] {
            b'`' => {
                if let Some(close) = find_byte(line, i + 1, b'`') {
                    out.push(Span { start: i, end: close + 1, tok: Tok::CodeSpan });
                    i = close + 1;
                } else {
                    i += 1;
                }
            }
            b'*' => {
                let mut run = 0;
                while i + run < n && line[i + run] == b'*' {
                    run += 1;
                }
                if run >= 2 {
                    if let Some(close) = find_run(line, i + run, b'*', 2) {
                        out.push(Span { start: i, end: close + 2, tok: Tok::Emphasis });
                        i = close + 2;
                    } else {
                        i += run;
                    }
                } else if let Some(close) = find_byte(line, i + 1, b'*') {
                    out.push(Span { start: i, end: close + 1, tok: Tok::Emphasis });
                    i = close + 1;
                } else {
                    i += 1;
                }
            }
            b'[' => {
                let mut matched = false;
                if let Some(rb) = find_byte(line, i + 1, b']') {
                    if rb + 1 < n && line[rb + 1] == b'(' {
                        if let Some(rp) = find_byte(line, rb + 2, b')') {
                            out.push(Span { start: i, end: rp + 1, tok: Tok::Link });
                            i = rp + 1;
                            matched = true;
                        }
                    }
                }
                if !matched {
                    i += 1;
                }
            }
            _ => {
                // plain text / invalid utf8 -> uncovered, advance one byte
                // at a time so we never straddle a span across it.
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(line: &str) -> Vec<Span> {
        let mut out = Vec::new();
        lex_rust_line(line.as_bytes(), &mut out);
        out
    }

    fn mtoks(line: &str) -> Vec<Span> {
        let mut out = Vec::new();
        lex_markdown_line(line.as_bytes(), &mut out);
        out
    }

    fn text_at<'a>(line: &'a str, s: &Span) -> &'a str {
        &line[s.start..s.end]
    }

    #[test]
    fn rust_empty_line() {
        assert!(toks("").is_empty());
    }

    #[test]
    fn rust_keywords_full_list() {
        for kw in RUST_KEYWORDS {
            let s = std::str::from_utf8(kw).unwrap();
            let line = format!("{s} x");
            let spans = toks(&line);
            assert_eq!(spans[0].tok, Tok::Keyword, "{s}");
            assert_eq!(text_at(&line, &spans[0]), s);
        }
    }

    #[test]
    fn rust_ident_vs_keyword() {
        let spans = toks("letter");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].tok, Tok::Ident);
    }

    #[test]
    fn rust_line_comment() {
        let spans = toks("let x = 1; // trailing comment");
        let c = spans.last().unwrap();
        assert_eq!(c.tok, Tok::Comment);
        assert_eq!(c.start, 11);
        assert_eq!(c.end, "let x = 1; // trailing comment".len());
    }

    #[test]
    fn rust_block_comment_same_line() {
        let line = "let x = /* hi */ 1;";
        let spans = toks(line);
        let c = spans.iter().find(|s| s.tok == Tok::Comment).unwrap();
        assert_eq!(text_at(line, c), "/* hi */");
    }

    #[test]
    fn rust_block_comment_unterminated_runs_to_eol() {
        let line = "let x = /* never closes";
        let spans = toks(line);
        let c = spans.iter().find(|s| s.tok == Tok::Comment).unwrap();
        assert_eq!(c.end, line.len());
    }

    #[test]
    fn rust_strings_and_unterminated() {
        let spans = toks(r#"let s = "hello";"#);
        let s = spans.iter().find(|s| s.tok == Tok::Str).unwrap();
        assert_eq!(text_at(r#"let s = "hello";"#, s), r#""hello""#);

        let line = r#"let s = "never closes"#;
        let spans = toks(line);
        let s = spans.iter().find(|s| s.tok == Tok::Str).unwrap();
        assert_eq!(s.end, line.len());
    }

    #[test]
    fn rust_string_escaped_quote_not_a_terminator() {
        let line = r#""a\"b""#;
        let spans = toks(line);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].tok, Tok::Str);
        assert_eq!(spans[0].end, line.len());
    }

    #[test]
    fn rust_raw_strings() {
        let line = r####"r"plain raw""####;
        let spans = toks(line);
        assert_eq!(spans[0].tok, Tok::Str);
        assert_eq!(text_at(line, &spans[0]), line);

        let line2 = r####"r#"has "inner" quotes"#"####;
        let spans2 = toks(line2);
        assert_eq!(spans2[0].tok, Tok::Str);
        assert_eq!(text_at(line2, &spans2[0]), line2);

        let line3 = r#"br"raw bytes""#;
        let spans3 = toks(line3);
        assert_eq!(spans3[0].tok, Tok::Str);
        assert_eq!(text_at(line3, &spans3[0]), line3);
    }

    #[test]
    fn rust_byte_string_and_char() {
        let line = r#"b"bytes""#;
        let spans = toks(line);
        assert_eq!(spans[0].tok, Tok::Str);
        assert_eq!(text_at(line, &spans[0]), line);

        let line2 = "b'a'";
        let spans2 = toks(line2);
        assert_eq!(spans2[0].tok, Tok::Str);
        assert_eq!(text_at(line2, &spans2[0]), line2);
    }

    #[test]
    fn rust_char_literal_and_escapes() {
        let spans = toks("'a'");
        assert_eq!(spans[0].tok, Tok::Str);
        assert_eq!(spans[0].end, 3);

        let spans = toks(r"'\n'");
        assert_eq!(spans[0].tok, Tok::Str);
        assert_eq!(spans[0].end, 4);

        let spans = toks(r"'\''");
        assert_eq!(spans[0].tok, Tok::Str);
        assert_eq!(spans[0].end, 4);
    }

    #[test]
    fn rust_lifetime_is_ident() {
        let line = "fn f<'a>(x: &'a str)";
        let spans = toks(line);
        let life = spans.iter().find(|s| text_at(line, s) == "'a").unwrap();
        assert_eq!(life.tok, Tok::Ident);
    }

    #[test]
    fn rust_loop_label() {
        let line = "'outer: loop {}";
        let spans = toks(line);
        assert_eq!(spans[0].tok, Tok::Ident);
        assert_eq!(text_at(line, &spans[0]), "'outer");
    }

    #[test]
    fn rust_numbers() {
        for (src, want_end) in [
            ("42", 2),
            ("42u32", 5),
            ("3.14", 4),
            ("3.", 1), // trailing dot without digit is not consumed as float
            ("1_000_000", 9),
            ("0xFF_AA", 7),
            ("0o17", 4),
            ("0b1010_1", 8),
            ("1e10", 4),
            ("1.5e-3", 6),
        ] {
            let spans = toks(src);
            assert_eq!(spans[0].tok, Tok::Number, "{src}");
            assert_eq!(spans[0].end, want_end, "{src}");
        }
    }

    #[test]
    fn rust_range_dot_not_swallowed() {
        // `1..2` : number `1`, then punct `..`, then number `2`.
        let spans = toks("1..2");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].tok, Tok::Number);
        assert_eq!(spans[0].end, 1);
        assert_eq!(spans[1].tok, Tok::Punct);
        assert_eq!(spans[2].tok, Tok::Number);
    }

    #[test]
    fn rust_punctuation_merged() {
        let spans = toks("a -> b");
        let p = spans.iter().find(|s| s.tok == Tok::Punct).unwrap();
        assert_eq!(text_at("a -> b", p), "->");
    }

    #[test]
    fn rust_invalid_utf8_no_panic() {
        let line: &[u8] = b"let x = \xff\xfe 1;";
        let mut out = Vec::new();
        lex_rust_line(line, &mut out);
        // must not panic; invalid bytes are simply left uncovered
        assert!(out.iter().all(|s| s.start <= s.end && s.end <= line.len()));
    }

    #[test]
    fn rust_spans_ascending_non_overlapping() {
        let line = "fn main() { let x: u32 = 0xFF; } // done";
        let spans = toks(line);
        for w in spans.windows(2) {
            assert!(w[0].end <= w[1].start);
        }
    }

    #[test]
    fn md_empty_line() {
        assert!(mtoks("").is_empty());
    }

    #[test]
    fn md_heading_levels() {
        for src in ["# H1", "## H2", "###### H6"] {
            let spans = mtoks(src);
            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0].tok, Tok::Heading);
            assert_eq!(spans[0].end, src.len());
        }
        // 7 '#'s is not a heading (not covered at all -> Text/uncovered)
        let spans = mtoks("####### not a heading");
        assert!(spans.is_empty());
        // '#' with no following space is not a heading
        let spans = mtoks("#nospace");
        assert!(spans.is_empty());
    }

    #[test]
    fn md_fenced_code_line() {
        let spans = mtoks("```rust");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].tok, Tok::CodeSpan);
        assert_eq!(spans[0].end, "```rust".len());
    }

    #[test]
    fn md_code_span() {
        let line = "run `cargo test` now";
        let spans = mtoks(line);
        let c = spans.iter().find(|s| s.tok == Tok::CodeSpan).unwrap();
        assert_eq!(text_at(line, c), "`cargo test`");
    }

    #[test]
    fn md_unterminated_backtick_is_text() {
        let spans = mtoks("open `never closes");
        assert!(spans.iter().all(|s| s.tok != Tok::CodeSpan));
    }

    #[test]
    fn md_emphasis_and_strong() {
        let line = "a *emph* b **strong** c";
        let spans = mtoks(line);
        let e = spans.iter().find(|s| text_at(line, s) == "*emph*").unwrap();
        assert_eq!(e.tok, Tok::Emphasis);
        let s = spans.iter().find(|s| text_at(line, s) == "**strong**").unwrap();
        assert_eq!(s.tok, Tok::Emphasis);
    }

    #[test]
    fn md_link() {
        let line = "see [teddy](https://example.com) here";
        let spans = mtoks(line);
        let l = spans.iter().find(|s| s.tok == Tok::Link).unwrap();
        assert_eq!(text_at(line, l), "[teddy](https://example.com)");
    }

    #[test]
    fn md_bracket_without_paren_is_not_link() {
        let spans = mtoks("[not a link] plain");
        assert!(spans.iter().all(|s| s.tok != Tok::Link));
    }

    #[test]
    fn md_invalid_utf8_no_panic() {
        let line: &[u8] = b"text \xff\xfe *emph* end";
        let mut out = Vec::new();
        lex_markdown_line(line, &mut out);
        assert!(out.iter().all(|s| s.start <= s.end && s.end <= line.len()));
    }

    #[test]
    fn md_spans_ascending_non_overlapping() {
        let line = "a `code` b *em* c [link](url) d";
        let spans = mtoks(line);
        for w in spans.windows(2) {
            assert!(w[0].end <= w[1].start);
        }
    }
}
