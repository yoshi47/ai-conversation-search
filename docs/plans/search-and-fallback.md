# 検索意味論の修正 — AND試行→ORフォールバック + タイトル重み + Skill初手の修正

> **For agentic workers:** TDD で1タスクずつ。各ステップは `- [ ]`。

**Goal:** 複数語クエリの既定をANDに戻し、0件のときだけORにフォールバックする。v0.14.0でAND→ORに変えた理由(ANDだと0件になりやすい)を壊さずに、`crit markdown browser`のような探索クエリのノイズを消す。あわせてタイトル一致の重み付けとSkill Level 1の初手(`--days 14`廃止・`--group-by-session`化)を行う。

**Non-goal:** ベクトル検索(sqlite-vec)、tokenizer変更(bigramは別プラン `bigram-short-terms.md`)、LLMタグ付け・要約強化。新テーブル・再インデックスなし。`--exact`と明示演算子(`AND`/`OR`/`NOT`)の挙動は変えない。

**Architecture:** `src/search.rs:plan_query`と`search_conversations`/`search_grouped_by_session`の2経路のみ。Rust側リランクで重み付けし、FTSスキーマ変更なし。

---

## Context

- `plan_query`はHybridで長語(3文字以上)を`"... " OR "..."`連結する(`src/search.rs:319-327`)。`crit markdown browser`→`"crit" OR "markdown" OR "browser"`。どれか1語でヒットしbm25順。
- v0.14.0でAND→ORに変えた理由は`パッケージ アップグレード ドキュメント`の0件回避(`CHANGELOG.md:[0.14.0]`)。単純revertはこの回帰を再発させる。
- `message_content_fts`は単一カラム(`message_uuid UNINDEXED, full_content`)で列重み(`bm25(tbl, w1, w2)`)が使えない(`data/schema.sql:50-56`)。タイトルは別クエリで合流(`inject_title_matches`、`src/search.rs:822-828`)し重みなし。
- Skill Level 1は`search "terms" --days 14 --json`(`skills/conversation-search/SKILL.md:157`)。6/29・7/28のような古いセッションは初手で除外される。CLI既定は`days_back: None`=全期間(`src/search.rs:34-48`)。
- `--group-by-session`は存在するがLevel 1例文にない。メッセージ単位の大量ヒットで埋まる。

## 設計判断

- **AND試行→ORフォールバックの二段構え。** 1段目: 全長語をAND(`"a" AND "b"`)でFTS。ヒット0なら2段目: 従来ORで再実行。v0.14.0の0件問題は2段目で吸収する。この判断理由をコードコメントに残す。
- **重みはRust側。** FTSスキーマを変えないため、タイトル一致行にスコアボーナス(`-0.5`等の定数ではなく順序規則: AND本文 > ANDタイトル > OR本文 > ORタイトル)を付ける。bm25値の直接加算はスケール依存で不安定なため順序規則に留める。
- **短語(`mo`等2文字以下)の扱いは変えない。** phase-2のLIKE減算のまま。本プランでは対象外(bigram側で救済)。
- **明示演算子・`--exact`・引用句はRawのまま。** `title_terms`のスキップ規則(`src/search.rs:1121-1130`)も維持。`NOT`反転事故を起こさない。
- **Skillはドキュメントのみ。** Rust無変更で初手を全期間+集約化する。

---

## Task 1: AND→ORフォールバック(FTS経路)

**Files:** `src/search.rs`

- [ ] **Step 1: `plan_query`にAND形を追加。** Hybridが`fts_query_and`(`"a" AND "b"`)と`fts_query_or`(`"a" OR "b"`)の両方を返すようにする。`Raw`/`LikeOnly`は変更なし。
- [ ] **Step 2: `search_conversations`で二段実行。** 1段目AND(`query_fts_rowids(&and_q)`)→phase-2フィルタ→0件なら2段目ORで同じphase-2を再実行。`title_terms`合流(`inject_title_matches`)は両段の後に1回だけ。
- [ ] **Step 3: `search_grouped_by_session`も同形に。** `match_count`/`total_matched_messages`は最終段(実際に返した段)基準。全バッチ走査の不変条件(早期stopなし)を維持。
- [ ] **Step 4: テスト:**
  - 全語を含む行だけが1段目で返る(AND semantics)
  - 1段目0件でORに落ちて件数が増える(fallback)
  - 単語1つ・`--exact`・明示`AND`/`OR`/`NOT`は従来通り
  - 短語混じり(`デプロイ 失敗`)のHybrid+LIKE減算が維持される
  - 既存の境界テスト(`test_search_truncation_boundary_exact_limit`相当)が緑

## Task 2: タイトル重みの順序規則

**Files:** `src/search.rs`

- [ ] **Step 1: 4階層の順序規則を実装。** `Relevance`ソート時: (1)AND本文 (2)ANDタイトル (3)OR本文 (4)ORタイトル、各層内はbm25→timestamp。`Recent`ソート時は従来通りtimestampのみ。
- [ ] **Step 2: groupedの代表選定と一致させる。** セッションの順位=最良層・最良bm25、代表=そのメッセージ(タイトル層ならタイトル行)。代表ハイジャック防止の既存不変条件を維持。
- [ ] **Step 3: テスト:**
  - 本文AND行がタイトル行より上位に来る
  - ORフォールバック時も本文がタイトルのみ一致より上位
  - `--sort recent`では順序規則が効かない

## Task 3: Skill初手の修正

**Files:** `skills/conversation-search/SKILL.md`、`skills/conversation-search/REFERENCE.md`

- [ ] **Step 1: Level 1例文の置換。** Topic/Hybrid初手を`search "terms" --days 14 --json`→`search "terms" --group-by-session --limit 50 --json`(期間フィルタなし)に。Temporalは`list`のまま。
- [ ] **Step 2: 注意書き1行の追加。** 「昔/以前/あの」が付いたら期間フィルタ禁止。`--days`は日付が明示されたときのみ。狭い範囲のまま断定しない(既存のLevel 4注意と整合)。
- [ ] **Step 3: REFERENCE.mdにAND→ORフォールバックの説明。** 既定AND、0件時OR、絞りたいときは語を足す・広げたいときは語を引く/`--exact`の使い分け。
- [ ] **Step 4: CHANGELOG Unreleasedに記載。** AND復帰ではなくフォールバックである旨、v0.14.0との関係を明記。

---

## Verification

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build --release
./target/release/ai-conversation-search search "inkmark crit" --group-by-session --limit 50 --json 2>/dev/null | jq '.results | length'
./target/release/ai-conversation-search search "パッケージ アップグレード ドキュメント" --json 2>/dev/null | jq '.results | length'
```

期待値: 前者はANDで絞られてノイズ減、後者はAND 0件→ORフォールバックで件数>0。既存の0件になりやすいクエリが0件に戻らない。

---

## Ripple（別プランへの波及）

- `bigram-short-terms.md`: 短語のLIKE減算は本プランで温存。bigram側が救済したらphase-2条件の見直し余地あり。
- `query-expansion.md`: 拡張語の融合は本プランのAND/OR二段の上に載せる(拡張語はOR側に足す)。
