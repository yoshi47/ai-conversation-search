//! Bigram analyzer for short-term / CJK rescue.
//!
//! Design note (`docs/plans/bigram-short-terms.md`):
//! FTS5 `tokenize=` is per-table, so the existing trigram table cannot index
//! 2-char tokens. This module pre-tokenizes in Rust: CJK runs become
//! space-separated overlapping bigrams fed to a `unicode61` table, ASCII stays
//! as words (queried with `prefix*`). Index and query go through the same
//! analysis. One-char runs and non-tokenizable input yield nothing (caller
//! keeps them as the phase-2 LIKE residual).

/// CJK ranges fixed by the plan: Hiragana, Katakana, Han, Hangul.
///
/// Halfwidth Katakana (U+FF61-FF9F) is intentionally NOT included: it lives in
/// the fullwidth ASCII block and is treated as a separator, same as
/// punctuation / emoji / control chars.
pub fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{3041}'..='\u{3096}'   // Hiragana
        | '\u{30A1}'..='\u{30FF}' // Katakana (fullwidth only)
        | '\u{3400}'..='\u{4DBF}' // CJK Ext A
        | '\u{4E00}'..='\u{9FFF}' // CJK Unified
        | '\u{F900}'..='\u{FAFF}' // CJK Compatibility
        | '\u{20000}'..='\u{2EBEF}' // CJK Ext B-F
        | '\u{AC00}'..='\u{D7AF}' // Hangul Syllables
        | '\u{1100}'..='\u{11FF}' // Hangul Jamo
        | '\u{3130}'..='\u{318F}' // Hangul Compatibility Jamo
    )
}

/// Normalize fullwidth ASCII (U+FF01-FF5E) to ASCII, fullwidth space (U+3000)
/// to a plain space. Everything else passes through unchanged.
fn normalize_char(ch: char) -> char {
    if ch == '\u{3000}' {
        ' '
    } else if ('\u{FF01}'..='\u{FF5E}').contains(&ch) {
        // `0xFEE0` offset is the defined fullwidth→ASCII mapping.
        char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch)
    } else {
        ch
    }
}

fn is_ascii_word(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn escape_phrase(s: &str) -> String {
    s.replace('"', "\"\"")
}

/// Analyze document text for indexing: CJK runs become overlapping bigrams,
/// ASCII stays as space-separated words. Single-char CJK runs emit nothing.
///
/// # Examples
/// ```text
/// "全文检索" -> "全文 文检 检索"
/// "k1LoW/mo markdown" -> "k1LoW mo markdown"
/// ```
pub fn analyze_for_index(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut cjk_run: Vec<char> = Vec::new();
    let mut ascii_buf = String::new();

    let flush_ascii = |ascii_buf: &mut String, out: &mut Vec<String>| {
        if !ascii_buf.is_empty() {
            out.push(std::mem::take(ascii_buf));
        }
    };
    let flush_cjk = |cjk_run: &mut Vec<char>, out: &mut Vec<String>| {
        if cjk_run.len() >= 2 {
            for w in cjk_run.windows(2) {
                out.push(format!("{}{}", w[0], w[1]));
            }
        }
        cjk_run.clear();
    };

    for raw in text.chars() {
        let ch = normalize_char(raw);
        if is_cjk(ch) {
            flush_ascii(&mut ascii_buf, &mut out);
            cjk_run.push(ch);
        } else if is_ascii_word(ch) {
            flush_cjk(&mut cjk_run, &mut out);
            ascii_buf.push(ch);
        } else {
            // Separator: halfwidth kana, punctuation, emoji, controls, CJK
            // symbols, whitespace, `/`, etc. Ends any open run.
            flush_ascii(&mut ascii_buf, &mut out);
            flush_cjk(&mut cjk_run, &mut out);
        }
    }
    flush_ascii(&mut ascii_buf, &mut out);
    flush_cjk(&mut cjk_run, &mut out);
    out.join(" ")
}

/// FTS5 operator keywords must never be emitted as bare prefix queries
/// (`AND*` is a syntax error / operator injection).
fn is_fts_operator(token: &str) -> bool {
    matches!(
        token.to_ascii_uppercase().as_str(),
        "AND" | "OR" | "NOT" | "NEAR"
    )
}

/// Analyze a single whitespace-separated query term into an FTS5 `MATCH`
/// fragment for the bigram table, or `None` when the term cannot be expressed
/// in FTS (single char, emoji/symbol-only, ...). The caller keeps `None` terms
/// as the phase-2 LIKE residual.
///
/// Rules (fixed by the plan):
/// - 2-char CJK / 2-char ASCII → one quoted token (`"失敗"`, `"mo"`)
/// - 3+-char CJK → quoted-bigram AND (`"全文" AND "文检" AND "检索"`, not a phrase)
/// - 3+-char ASCII → bare prefix (`markdown*`, never `"markdown"*`)
pub fn analyze_for_query(term: &str) -> Option<String> {
    let normalized: String = term.chars().map(normalize_char).collect();
    if normalized.trim().is_empty() {
        return None;
    }

    // Split the term into CJK runs and ASCII tokens with the same scanning as
    // the index path, so `k1LoW/mo` (one query term) behaves like indexed text.
    let mut cjk_runs: Vec<Vec<char>> = Vec::new();
    let mut ascii_tokens: Vec<String> = Vec::new();
    let mut cur_cjk: Vec<char> = Vec::new();
    let mut cur_ascii = String::new();

    for ch in normalized.chars() {
        if is_cjk(ch) {
            if !cur_ascii.is_empty() {
                ascii_tokens.push(std::mem::take(&mut cur_ascii));
            }
            cur_cjk.push(ch);
        } else if is_ascii_word(ch) {
            if !cur_cjk.is_empty() {
                cjk_runs.push(std::mem::take(&mut cur_cjk));
            }
            cur_ascii.push(ch);
        } else {
            // Separator inside the term (`/`, `"`, `*`, emoji, whitespace,
            // ...). It ends runs but is not itself tokenizable. A "term"
            // should not contain whitespace, but be lenient.
            if !cur_cjk.is_empty() {
                cjk_runs.push(std::mem::take(&mut cur_cjk));
            }
            if !cur_ascii.is_empty() {
                ascii_tokens.push(std::mem::take(&mut cur_ascii));
            }
        }
    }
    if !cur_cjk.is_empty() {
        cjk_runs.push(cur_cjk);
    }
    if !cur_ascii.is_empty() {
        ascii_tokens.push(cur_ascii);
    }
    if cjk_runs.is_empty() && ascii_tokens.is_empty() {
        return None;
    }

    let mut fragments: Vec<String> = Vec::new();

    for run in &cjk_runs {
        if run.len() < 2 {
            continue; // single-char CJK → LIKE residual
        }
        for w in run.windows(2) {
            let bigram = format!("{}{}", w[0], w[1]);
            fragments.push(format!("\"{}\"", escape_phrase(&bigram)));
        }
    }

    for token in &ascii_tokens {
        let n = token.chars().count();
        if n < 2 {
            continue; // single-char ASCII → LIKE residual
        }
        if n == 2 || is_fts_operator(token) {
            fragments.push(format!("\"{}\"", escape_phrase(token)));
        } else {
            // Bare prefix query. Strip everything outside [A-Za-z0-9_] so a
            // `*` / `"` / paren inside the term cannot break FTS syntax.
            let clean: String = token
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if clean.chars().count() < 2 {
                continue;
            }
            if is_fts_operator(&clean) {
                fragments.push(format!("\"{}\"", escape_phrase(&clean)));
            } else {
                fragments.push(format!("{}*", clean));
            }
        }
    }

    if fragments.is_empty() {
        None
    } else {
        Some(fragments.join(" AND "))
    }
}

/// Register `bigram_analyze(text)` as a SQLite scalar function on `conn`.
///
/// `schema::drain_bigram_pending` and `schema::fill_bigram_missing` call this
/// to fill the bigram FTS table from `messages.full_content`. The FTS sync
/// triggers must NOT call it: the index DB is shared with older binaries that
/// never register it, and every write of theirs would fail. They queue rows
/// instead, so all three indexers still share a single write path.
/// No SQLite extension (`.so`) is involved.
///
/// Production connections get it via `db::connect`; test / in-memory
/// connections via `schema::init_schema`.
pub fn register_sql_function(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "bigram_analyze",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            Ok(analyze_for_index(&text))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_keeps_two_char_terms() {
        assert_eq!(analyze_for_index("mo"), "mo");
        assert_eq!(analyze_for_index("k1LoW/mo markdown"), "k1LoW mo markdown");
    }

    #[test]
    fn test_index_cjk_bigrams() {
        assert_eq!(analyze_for_index("失敗"), "失敗");
        assert_eq!(analyze_for_index("全文检索"), "全文 文检 检索");
        // 4-char Japanese: 3 overlapping bigrams.
        assert_eq!(analyze_for_index("検索失敗"), "検索 索失 失敗");
    }

    #[test]
    fn test_index_mixed_ascii_cjk() {
        assert_eq!(analyze_for_index("k1LoW mo markdown"), "k1LoW mo markdown");
        let out = analyze_for_index("mo失敗");
        assert_eq!(out, "mo 失敗");
    }

    #[test]
    fn test_query_two_char_terms() {
        assert_eq!(analyze_for_query("mo"), Some("\"mo\"".to_string()));
        assert_eq!(analyze_for_query("失敗"), Some("\"失敗\"".to_string()));
    }

    #[test]
    fn test_query_long_cjk_is_bigram_and() {
        assert_eq!(
            analyze_for_query("全文检索"),
            Some("\"全文\" AND \"文检\" AND \"检索\"".to_string())
        );
    }

    #[test]
    fn test_query_ascii_prefix() {
        assert_eq!(analyze_for_query("markdown"), Some("markdown*".to_string()));
        assert_eq!(analyze_for_query("k1LoW"), Some("k1LoW*".to_string()));
        // Two-char ASCII stays quoted, never prefix.
        assert_eq!(analyze_for_query("mo"), Some("\"mo\"".to_string()));
    }

    #[test]
    fn test_query_single_char_yields_none() {
        assert_eq!(analyze_for_query("あ"), None);
        assert_eq!(analyze_for_query("a"), None);
        assert_eq!(analyze_for_query(""), None);
        assert_eq!(analyze_for_query("   "), None);
    }

    #[test]
    fn test_no_panic_on_edge_input() {
        for s in ["", " ", "😀", "😀😀😀", "\0\x01\x02", "ｱｲｳ", "、。，", "。"] {
            let _ = analyze_for_index(s);
            let _ = analyze_for_query(s);
        }
        // Halfwidth kana are separators, not CJK.
        assert_eq!(analyze_for_index("ｱｲｳ"), "");
        assert_eq!(analyze_for_query("ｱｲｳ"), None);
        // Fullwidth ASCII normalizes.
        assert_eq!(analyze_for_index("ｍｏ"), "mo");
        assert_eq!(analyze_for_query("ｍｏ"), Some("\"mo\"".to_string()));
    }

    #[test]
    fn test_query_escaping() {
        // `"` and `*` inside a term are separators: they must not panic, and
        // any emitted fragment must be valid FTS5 syntax (verified below by
        // executing the MATCH). `a"b` splits into single chars → LIKE residual.
        assert_eq!(analyze_for_query("a\"b"), None);
        let frag = analyze_for_query("ab\"cd").unwrap();
        assert_eq!(frag, "\"ab\" AND \"cd\"");
        // `*` acts as a separator; each side becomes its own prefix query.
        let q = analyze_for_query("mark*down").unwrap();
        assert_eq!(q, "mark* AND down*");
        // Operator keywords are quoted, never emitted bare.
        assert_eq!(analyze_for_query("AND"), Some("\"AND\"".to_string()));
        assert_eq!(analyze_for_query("OR"), Some("\"OR\"".to_string()));
        // Every fragment produced here must execute without syntax error.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE e USING fts5(x, tokenize='unicode61 remove_diacritics 1');",
        )
        .unwrap();
        conn.execute("INSERT INTO e(x) VALUES ('ab cd mark markdown')", [])
            .unwrap();
        for term in ["ab\"cd", "mark*down", "AND", "OR", "a\"b"] {
            if let Some(f) = analyze_for_query(term) {
                let _: i64 = conn
                    .query_row("SELECT COUNT(*) FROM e WHERE x MATCH ?", [f], |r| r.get(0))
                    .unwrap_or_else(|e| panic!("fragment for {:?} failed: {}", term, e));
            }
        }
    }

    /// `bundled` rusqlite must ship both tokenizers this feature needs.
    ///
    /// NOTE: option syntax is space-separated (`remove_diacritics 1`); the
    /// `remove_diacritics=1` form fails with `parse error in tokenize
    /// directive` on the bundled SQLite 3.51.1.
    #[test]
    fn test_bundled_fts5_tokenizers_available() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for tokenize in ["unicode61 remove_diacritics 1", "trigram case_sensitive 0"] {
            let ddl = format!(
                "CREATE VIRTUAL TABLE f USING fts5(x, tokenize='{}')",
                tokenize
            );
            conn.execute_batch(&ddl)
                .unwrap_or_else(|e| panic!("tokenizer missing ({:?}): {}", tokenize, e));
            conn.execute_batch("DROP TABLE f;").unwrap();
        }
    }

    /// End-to-end through a real `unicode61` table: the exact DDL shape the
    /// migration uses, driven by `analyze_for_index` output.
    #[test]
    fn test_bigram_table_roundtrip() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE b USING fts5(
                message_uuid UNINDEXED,
                bigram_content,
                tokenize='unicode61 remove_diacritics 1'
            );",
        )
        .unwrap();
        let docs = ["mo markdown memo", "認証に失敗した", "全文检索测试"];
        for (i, d) in docs.iter().enumerate() {
            let analyzed = analyze_for_index(d);
            conn.execute(
                "INSERT INTO b(message_uuid, bigram_content) VALUES (?, ?)",
                rusqlite::params![format!("m{}", i), analyzed],
            )
            .unwrap();
        }
        // 2-char queries hit via the analyzed form.
        for (query_term, expect) in [("mo", 1), ("失敗", 1), ("全文", 1)] {
            let frag = analyze_for_query(query_term).unwrap();
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM b WHERE bigram_content MATCH ?",
                    [frag],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, expect, "query {:?}", query_term);
        }
        // Long ASCII uses prefix.
        let frag = analyze_for_query("markdown").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM b WHERE bigram_content MATCH ?",
                [frag],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }
}
