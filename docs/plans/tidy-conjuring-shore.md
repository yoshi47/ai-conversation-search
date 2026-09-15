# タイトル（会話サマリ）検索対応 — テーマ#8 実装プラン

> **For agentic workers:** TDD で1タスクずつ。各ステップは `- [ ]`。

**Goal:** セッションのタイトル（`conversation_summary`）に一致するクエリで、本文に同語が無くても該当セッションを検索ヒットさせる。

**Architecture:** タイトルは `conversations` テーブルにあり messages FTS 索引に入っていない。本文の FTS 結果を作った後に、「本文でヒットしていないタイトル一致セッション」だけを会話単位フィルタで引き、代表メッセージ行として結果に合流させる（新テーブル・再インデックス・スキーマ変更なし）。

**Tech Stack:** Rust / rusqlite / SQLite FTS5 / chrono。

---

## Context

過去145セッションの監査で確定した唯一の「confirmed かつ Rust 変更が要る」フリクション。タイトルで覚えているセッションを検索しても0件になる。

**根本原因**（実コードで確認済み）:
- タイトルは `conversations.conversation_summary`（`data/schema.sql:83-98`）に格納。FTS 索引 `message_content_fts` は `messages.full_content` のみ対象（`src/search.rs:1100-1112`）。
- FTS 経路は phase-1（`query_fts_rowids`）が空なら早期 return（`src/search.rs:752-758` / grouped `947-955`）。本文非一致だとタイトル一致セッションはここで消える。phase-2 に `OR` を足しても phase-1 で 0 件のため届かない。
- 影響関数は2つ: `search_conversations`（`src/search.rs:678`）と `search_grouped_by_session`（`src/search.rs:865`）。同構造。
- **ピッカーの live 検索は `search --group-by-session --json`**（`bin/ai-conversation-search:248,290`）。fzf ヘッダ「type to search titles + content」（`:327`）は現状ウソ。本対応で真になる。

## 設計判断（レビュー反映済み — Critical 2件を回避する形）

**採用: 本文 FTS 結果を作った後、ソート直前に「タイトルオンリー行」を差し込む。**

タイトル一致は**セッション単位の事実**なので、1メッセージ行に代理させると2つの Critical が出る（初版で指摘）。それを避けるため:

- **セッション単位で重複排除**（Critical #1 回避）: 本文でヒット済みのセッション集合 `seen` を除外して、タイトル**のみ**一致するセッションだけを追加する。両方一致するセッションは本物の本文行がそのまま代表/上位に残る（`search_grouped_by_session` の代表選択不変条件 `src/search.rs:998-1000` を壊さない）。
- **日付/リポジトリ/ソースはセッション列で判定**（Critical #2 回避）: `title_only_rows` の SQL 内で `conversations.first_message_at`/`last_message_at`/`repo_root`/`source`/`project_path` に対してフィルタを適用。日付は「セッション活動期間と要求期間の重なり」で判定するため、代表メッセージのタイムスタンプに依存しない。
- **代表 = 最新の非メタ message**（相関サブクエリ）。resume は session_id 単位なので成立。`context_snippet` にタイトルを入れ「なぜヒットしたか」を可視化（構造レビュー Suggestion 反映）。
- タイトル一致は高精度な明示的意図なのでスコア `f64::NEG_INFINITY`（bm25 は負値ほど上位、`NEG_INFINITY.partial_cmp(&finite)` は常に `Some(Less)`、パニック無し ― `src/search.rs:827,1006` で検証）。
- **DRY**（Important #3）: 一致・重複排除・フィルタの実体は共有ヘルパ `title_only_rows` / `inject_title_matches` に集約し、両関数からは1行呼ぶだけ。リポジトリメモリ [[project_tech_debt_grouped_search]] の DRY 指摘に沿う。
- **引用句・FTS演算子クエリはスキップ**（Important #4）: `"foo bar"` や `AND/OR/NOT` を含むクエリは `title_terms` が空を返し、無意味な LIKE を撃たない。

**スコープ外（明記して棄却）:**
- LikeOnly 経路（全語が trigram 下限＝2文字以下、`src/search.rs:709,893`）のタイトル対応。極短語のタイトル検索は低価値・高ノイズ。
- `--exact` 厳密句でのタイトル照合。
- タイトル専用 FTS テーブル / 再インデックス（会話数が少なく LIKE で十分、YAGNI）。
- `match_count`/`total_matched_messages`（`src/search.rs:987-991`）はタイトルオンリー行を「1メッセージ一致」として数える。実本文一致ではない点は許容し、CHANGELOG に注記する（Important #5）。

## ブランチ・リリース戦略（ユーザーの質問への回答）

**推奨: doc 修正を先にコミット → #8 は別ブランチ、リリースは1回にまとめる。**

- 現ブランチ `docs/skill-friction-fixes` に SKILL.md 5件を先にコミット（Task 0）。docs 専用に保つ。
- #8（Rust）は `main` から `feat/title-search` を切って実装。レビュー単位を分離。
- 両方 `main` 投入後に **`bump-version.sh 0.16.2` を1回**・タグ1本。プラグイン更新も1回。
- 破壊的変更ではない（結果に行が増えるのみ）ので **patch**。

> #8 が想定より重い（2関数 + 3ヘルパ + セッション単位日付ロジック + 5テスト）ことが判明。もし #8 を後回しにしたければ、doc だけ 0.16.2 で先行リリースし #8 を 0.16.3 に回す選択も依然有効。既定は同梱。

---

## Task 0: doc 修正の確定（先行作業）

**Files:** `skills/conversation-search/SKILL.md`（実装・レビュー済み、未コミット）

- [ ] **Step 1: コミット**（現ブランチ `docs/skill-friction-fixes`）

```bash
git add skills/conversation-search/SKILL.md docs/plans/serialized-shimmying-pretzel.md
git commit -m "docs(skill): 監査で確定した5件の誤用フリクションを塞ぐ

過去145セッションの監査で verdict=confirmed だった doc-only の誤用パターン
（生jsonlのpython/jq直パース、--json のstderr混入、狭い範囲での断定、
TodoWrite不在時のMANDATORY矛盾、--no-tools/短縮ID resumeの罠）を SKILL.md で塞ぐ。
Rust 無変更。配布は 0.16.2 バンプで #8 と同梱。"
```

- [ ] **Step 2: #8 用ブランチを main から作成**

```bash
git checkout main && git checkout -b feat/title-search
```

（doc コミットは別途 `main` へ PR/マージ。#8 マージ後にまとめて 1 リリース。）

## Task 1: ヘルパ群を追加（クエリ・フィルタ・注入）

**Files:** `src/search.rs`（`query_fts_rowids`（`src/search.rs:1100`）の直前に追加）

まだ呼び出し側には配線しない（Task 2/3 で配線）。ここでは3ヘルパを定義しコンパイルを通す。

- [ ] **Step 1: `title_terms` を追加**（引用句・演算子クエリを弾く）

```rust
/// タイトル照合に使う語。引用句や FTS 演算子を含むクエリは対象外（空を返す）。
fn title_terms(trimmed: &str) -> Vec<&str> {
    if trimmed.contains('"') {
        return Vec::new();
    }
    trimmed
        .split_whitespace()
        .filter(|t| !matches!(*t, "AND" | "OR" | "NOT"))
        .collect()
}
```

- [ ] **Step 2: セッション単位フィルタ `append_conversation_filters` を追加**

```rust
/// conversations 行に対するフィルタ。日付は「セッション活動期間 [first,last] と
/// 要求期間の重なり」で判定する（タイトル一致は特定メッセージの時刻に紐づかない）。
fn append_conversation_filters(
    sql: &mut String,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    filter: &SearchFilter<'_>,
) -> Result<()> {
    use chrono::{NaiveTime, TimeDelta};

    if let Some(d) = filter.date {
        let start = crate::date_utils::parse_date(d)?;
        let end = start + TimeDelta::days(1);
        sql.push_str(" AND c.last_message_at >= ? AND c.first_message_at < ?");
        params.push(Box::new(start.and_time(NaiveTime::MIN).format("%Y-%m-%dT%H:%M:%S").to_string()));
        params.push(Box::new(end.and_time(NaiveTime::MIN).format("%Y-%m-%dT%H:%M:%S").to_string()));
    } else if filter.since.is_some() || filter.until.is_some() {
        if let Some(s) = filter.since {
            let start = crate::date_utils::parse_date(s)?.and_time(NaiveTime::MIN);
            sql.push_str(" AND c.last_message_at >= ?");
            params.push(Box::new(start.format("%Y-%m-%dT%H:%M:%S").to_string()));
        }
        if let Some(u) = filter.until {
            let end = (crate::date_utils::parse_date(u)? + TimeDelta::days(1)).and_time(NaiveTime::MIN);
            sql.push_str(" AND c.first_message_at < ?");
            params.push(Box::new(end.format("%Y-%m-%dT%H:%M:%S").to_string()));
        }
    } else if let Some(d) = filter.days_back {
        let cutoff = (chrono::Local::now() - TimeDelta::days(d)).naive_local();
        sql.push_str(" AND c.last_message_at >= ?");
        params.push(Box::new(cutoff.format("%Y-%m-%dT%H:%M:%S").to_string()));
    }

    if let Some(pp) = filter.project_path {
        sql.push_str(" AND c.project_path = ?");
        params.push(Box::new(pp.to_string()));
    }
    if let Some(r) = filter.repo {
        sql.push_str(" AND c.repo_root LIKE ? ESCAPE '\\'");
        params.push(Box::new(format!("%{}%", escape_like(r))));
    }
    if let Some(s) = filter.source {
        sql.push_str(" AND c.source = ?");
        params.push(Box::new(s.to_string()));
    }
    Ok(())
}
```

- [ ] **Step 3: `title_only_rows` を追加**（全語一致 + フィルタ + seen 除外）

```rust
/// タイトルが全語に一致し、セッション単位フィルタを通過する会話のうち、
/// seen_sessions に無いものを SearchResultRow で返す。代表は最新の非メタ message、
/// context_snippet はタイトル（なぜ一致したかを可視化）。
fn title_only_rows(
    &mut self,
    terms: &[&str],
    filter: &SearchFilter<'_>,
    seen_sessions: &std::collections::HashSet<String>,
) -> Result<Vec<SearchResultRow>> {
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = String::from(
        "SELECT m.rowid AS message_rowid, m.message_uuid, m.session_id, m.parent_uuid, \
                m.timestamp, m.message_type, m.project_path, m.depth, m.is_sidechain, \
                c.conversation_summary AS context_snippet, \
                c.conversation_summary, c.conversation_file, c.source \
         FROM conversations c \
         JOIN messages m ON m.message_uuid = ( \
             SELECT m2.message_uuid FROM messages m2 \
             WHERE m2.session_id = c.session_id AND m2.is_meta_conversation = FALSE \
             ORDER BY m2.rowid DESC LIMIT 1 ) \
         WHERE c.conversation_summary IS NOT NULL",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    for term in terms {
        sql.push_str(" AND c.conversation_summary LIKE ? ESCAPE '\\'");
        params.push(Box::new(format!("%{}%", escape_like(term))));
    }
    Self::append_conversation_filters(&mut sql, &mut params, filter)?;

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(|p| p.as_ref()).collect();
    let rows = self.query_rows(&sql, &param_refs, SearchResultRow::from_row)?;
    Ok(rows
        .into_iter()
        .filter(|r| !seen_sessions.contains(&r.session_id))
        .collect())
}
```

- [ ] **Step 4: `inject_title_matches` を追加**（両関数から呼ぶ配線ヘルパ）

```rust
/// 本文結果 all_results / score_by_rowid に、タイトルオンリー行を先頭合流する。
/// seen（本文ヒット済みセッション）を除外し、代表選択のハイジャックを防ぐ。
fn inject_title_matches(
    &mut self,
    trimmed: &str,
    filter: &SearchFilter<'_>,
    all_results: &mut Vec<SearchResultRow>,
    score_by_rowid: &mut HashMap<i64, f64>,
) -> Result<()> {
    let terms = Self::title_terms(trimmed);
    if terms.is_empty() {
        return Ok(());
    }
    let seen: std::collections::HashSet<String> =
        all_results.iter().map(|r| r.session_id.clone()).collect();
    let title_rows = self.title_only_rows(&terms, filter, &seen)?;
    for r in &title_rows {
        score_by_rowid.insert(r.rowid, f64::NEG_INFINITY);
    }
    all_results.splice(0..0, title_rows);
    Ok(())
}
```

- [ ] **Step 5: コンパイル確認**

Run: `cargo build`
Expected: 成功（未使用警告は Task 2/3 の配線で解消）。

## Task 2: `search_conversations`（flat）に配線

**Files:** `src/search.rs`（`760`, `816` 付近）

- [ ] **Step 1: 失敗テストを書く**（`test_search_fts_match`（`src/search.rs:3028`）の直後）

```rust
#[test]
fn test_search_matches_title_only() {
    let conn = setup_test_db();
    insert_test_conversation(
        &conn, "sess1", "/proj", "RedisMigration plan",
        "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code",
    );
    insert_test_message(
        &conn, "msg1", "sess1",
        "we discussed caching strategy at length", "user",
        "2025-01-15T10:00:00", "/proj",
    );

    let mut searcher = ConversationSearch::from_connection(conn);
    let results = searcher
        .search_conversations("RedisMigration", &default_filter())
        .unwrap().rows;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session_id, "sess1");
}

#[test]
fn test_title_and_body_match_no_duplicate() {
    let conn = setup_test_db();
    insert_test_conversation(
        &conn, "sess1", "/proj", "RedisMigration plan",
        "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code",
    );
    // 本文にも同語 → 本文ヒット。タイトルアンカーが重複追加されないこと。
    insert_test_message(
        &conn, "msg1", "sess1",
        "we finished the RedisMigration today", "user",
        "2025-01-15T10:00:00", "/proj",
    );

    let mut searcher = ConversationSearch::from_connection(conn);
    let results = searcher
        .search_conversations("RedisMigration", &default_filter())
        .unwrap().rows;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].message_uuid, "msg1"); // 合成アンカーではなく本文行
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test test_search_matches_title_only`
Expected: FAIL（0 件）

- [ ] **Step 3: `score_by_rowid` を可変化**（`src/search.rs:760`）

`let score_by_rowid: HashMap<i64, f64> = scored.iter().copied().collect();`
→ `let mut score_by_rowid: HashMap<i64, f64> = scored.iter().copied().collect();`

- [ ] **Step 4: phase-2 ループ直後・ソート直前に注入**（`src/search.rs:816` の `}`（ループ閉じ）と `823` の `match filter.sort` の間）

```rust
        }

        // タイトルのみ一致するセッションを本文結果に合流（本文ヒット済みは除外）。
        self.inject_title_matches(trimmed, filter, &mut all_results, &mut score_by_rowid)?;

        match filter.sort {
```

- [ ] **Step 5: テスト通過を確認**

Run: `cargo test test_search_matches_title_only test_title_and_body_match_no_duplicate`
Expected: PASS

- [ ] **Step 6: ミューテーション確認**

Step 4 の注入行を一時削除 → `test_search_matches_title_only` が FAIL することを確認 → 戻す（[[feedback_verify_tests_by_mutation]]）。

## Task 3: `search_grouped_by_session`（ピッカー経路）に配線

**Files:** `src/search.rs`（`1001` 付近）

- [ ] **Step 1: 失敗テストを書く**

```rust
#[test]
fn test_grouped_search_matches_title_only() {
    let conn = setup_test_db();
    insert_test_conversation(
        &conn, "sess1", "/proj", "RedisMigration plan",
        "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code",
    );
    insert_test_message(
        &conn, "msg1", "sess1",
        "we discussed caching strategy at length", "user",
        "2025-01-15T10:00:00", "/proj",
    );

    let mut searcher = ConversationSearch::from_connection(conn);
    let results = searcher
        .search_grouped_by_session("RedisMigration", &default_filter())
        .unwrap().rows;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].representative.session_id, "sess1");
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test test_grouped_search_matches_title_only`
Expected: FAIL

- [ ] **Step 3: `score_by_rowid` を可変化 + 注入**（`src/search.rs:1001`）

`let score_by_rowid: HashMap<i64, f64> = scored.iter().copied().collect();`
→ 下記に差し替え（直後の `match filter.sort`（`1002`）の前に注入）:

```rust
        let mut score_by_rowid: HashMap<i64, f64> = scored.iter().copied().collect();
        self.inject_title_matches(trimmed, filter, &mut all_results, &mut score_by_rowid)?;
```

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test test_grouped_search_matches_title_only`
Expected: PASS

## Task 4: フィルタ回帰テスト（Critical #2 の担保）

**Files:** `src/search.rs`（テスト追加）

- [ ] **Step 1: リポジトリ/日付フィルタがタイトル一致にも効くテスト**

```rust
#[test]
fn test_title_match_respects_repo_filter() {
    let conn = setup_test_db();
    insert_test_conversation_with_repo(
        &conn, "sess1", "/proj", "RedisMigration plan",
        "2025-01-15T09:00:00", "2025-01-15T11:00:00", "claude_code", "/repos/alpha",
    );
    insert_test_message(&conn, "msg1", "sess1",
        "unrelated body text", "user", "2025-01-15T10:00:00", "/proj");

    let mut searcher = ConversationSearch::from_connection(conn);

    let mut f = default_filter();
    f.repo = Some("beta"); // 別リポジトリ → 除外
    assert_eq!(searcher.search_conversations("RedisMigration", &f).unwrap().rows.len(), 0);

    let mut f2 = default_filter();
    f2.repo = Some("alpha"); // 一致 → ヒット
    assert_eq!(searcher.search_conversations("RedisMigration", &f2).unwrap().rows.len(), 1);
}

#[test]
fn test_title_match_date_uses_session_range() {
    // 代表は最新メッセージ(Jan-10)だが、セッション活動期間 [Jan-01, Jan-10] が
    // --until Jan-05 と重なるので含めるべき（メッセージ時刻依存だと誤って落ちる）。
    let conn = setup_test_db();
    conn.execute(
        "INSERT INTO conversations (session_id, project_path, conversation_file, root_message_uuid, conversation_summary, first_message_at, last_message_at, message_count, source) \
         VALUES ('sess1', '/proj', 'test.jsonl', 'm1', 'RedisMigration', '2025-01-01T00:00:00', '2025-01-10T00:00:00', 2, 'claude_code')",
        [],
    ).unwrap();
    insert_test_message(&conn, "m1", "sess1", "early body", "user", "2025-01-01T00:00:00", "/proj");
    insert_test_message(&conn, "m2", "sess1", "late body",  "user", "2025-01-10T00:00:00", "/proj");

    let mut searcher = ConversationSearch::from_connection(conn);
    let mut f = default_filter();
    f.until = Some("2025-01-05");
    let rows = searcher.search_conversations("RedisMigration", &f).unwrap().rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, "sess1");
}
```

- [ ] **Step 2: 全テスト + コミット**

```bash
cargo test
git add src/search.rs
git commit -m "feat(search): タイトル(会話サマリ)一致を検索結果に含める

タイトルは messages FTS 索引に無く本文非一致だと0件になっていた。本文 FTS 結果を
作った後、本文ヒット済みを除いたタイトルオンリー行をセッション単位フィルタで引いて
合流。日付/リポジトリはセッション列で判定し、代表選択のハイジャックを防ぐ。"
```

## Task 5: ピッカーのヘルプ文言を実態に合わせる

**Files:** `bin/ai-conversation-search`（`180-181`）

`327` のヘッダ「type to search titles + content」は Task 3 で正しくなる（変更不要）。ヘルプ本文だけ更新。

- [ ] **Step 1:** `bin/ai-conversation-search:180-181` の `full-text search against message bodies` を `full-text search against session titles and message bodies` に修正。

- [ ] **Step 2: wrapper テスト**（CI外・要ビルド、CLAUDE.md 手順）

```bash
cargo build --release
ACS_TEST_BINARY="$PWD/target/release/ai-conversation-search" sh tests/test_pick.sh
```

Expected: PASS

- [ ] **Step 3: コミット**

```bash
git add bin/ai-conversation-search
git commit -m "docs(pick): live 検索がタイトルも対象になった旨をヘルプに反映"
```

## Task 6: レビュー・リリース（doc + title をまとめて）

- [ ] **Step 1: pr-review-toolkit:review-pr**（Rust 変更のため必須。Critical 修正・Important 判断・スコープ外はメモリ保存）
- [ ] **Step 2: doc ブランチと feat ブランチを main へ**（両方マージ）
- [ ] **Step 3: CHANGELOG.md に 0.16.2 節**（doc 5件 + title 検索。`match_count` はタイトルオンリー行を1件と数える旨を注記。自動リリースノートは該当節から生成: `aab5fbb`）
- [ ] **Step 4: バンプ** `./scripts/bump-version.sh 0.16.2`
- [ ] **Step 5: skill-discovery 手動チェック**（`tests/skill-discovery/scenarios.md` から1シナリオ）
- [ ] **Step 6: タグ push でリリース**（**outward-facing。実行前にユーザー確認**）

```bash
git commit -am "chore: bump version to 0.16.2"
git tag v0.16.2
git push origin v0.16.2
```

- [ ] **Step 7: 周知** — スキル markdown はバイナリ自己更新では届かない。ユーザーはプラグイン/marketplace 更新が必要。

## Verification

1. `cargo test`（新規5テスト含む全緑）。特に `test_title_and_body_match_no_duplicate`（ハイジャック無し）と `test_title_match_date_uses_session_range`（Critical #2）。
2. 実 DB: 既知タイトルのセッションを1つ選び、本文に出ない固有語で
   `ai-conversation-search search "<タイトル固有語>" --json` が該当 session_id を返す。返らなければ `ai-conversation-search index` 後に再確認。
3. `ai-conversation-search pick "<タイトル固有語>"` で該当セッションが候補に出る。
4. 既存本文検索が回帰しない（`test_search_fts_match` 等の従来テスト緑）。
