# 軽セマンティック — クエリ拡張(`--expand`)でrecallを上げる

> **For agentic workers:** TDD で1タスクずつ。各ステップは `- [ ]`。

**Goal:** 言葉の不一致(「ブラウザでMarkdown開く」vs本文の`open directory`、`比較`vs`使い分け`)による0件・誤ヒットを、クエリ拡張で救済する。インデックスにベクトルを持たず、クエリ文面だけを外部に投げる方式(参考: `yaalsn/ccsearch -e`)。新規依存なし・既定オフ・失敗時はexact-onlyに劣化する。

**Non-goal:** sqlite-vec/FAISSの同梱、embeddingの永続化・バックフィル、要約・タグのLLM化。本文の送信、自動での外部呼び出し。既定オン化。

**Architecture:** `search`に`--expand`フラグを追加するだけ。展開器は既存のClaude CLIログイン(`claude -p`)を流用し、`CCS_EXPAND_CMD`で差し替え可能にする。exact結果と拡張結果をRRFで融合する。Rust側のみ、スキーマ変更なし。

---

## Context

- FTSはトークンマッチのみ。「あのMarkdownブラウザツール」のような言い換え・固有名詞の揺れ(`mo`/`k1LoW/mo`/`Diamond Markdown`)は拾えない。前回調査の根本原因5番(エンティティ抽出なし)の軽量回答が本プラン。
- 重い派の前例: `claude-code-recall`はFTS+`jina-embeddings-v2-small-en`をRRF融合しembed-on-write+opt-in backfill。精度は高いが`fastembed`+`sqlite-vec`+`numpy`依存と運用(劣化検知・backfillスロットリング)が要る。acsの単一バイナリ・$0・オフライン方針とは相性が悪い。
- 軽い派の前例: `ccsearch -e`はクエリ拡張(類語・言い換え・仮説的抜粋)をHaikuに投げ、exact結果とfuseする。本文は送らずクエリ文面のみ。`CCS_EXPAND_CMD`にローカルモデルを指せば完全オフライン・約1秒。
- acsのSkillは`search --json`を`2>/dev/null | jq`で読む契約(`SKILL.md: Command Reference`)。stderrへの診断混入は禁止。`--expand`の遅延・失敗はこの契約を壊さない形にする。
- `claude_cmd()`が既に`CC_CONVERSATION_SEARCH_CMD`環境変数で差し替え可能(`src/cli.rs:40-42`)。展開器の指定も同型の環境変数で足並みを揃える。

## 設計判断

- **既定オフ。** `--expand`を付けたときだけ外部呼び出しする。SkillのLevel 1にも入れない(Level 2以降の「広げる」手段として記載)。不意の課金・遅延・外部送信を避ける。
- **送るのはクエリ文面のみ。** 本文・パス・セッションIDは送らない。プロンプトは固定文で、出力はカンマ区切り語彙に制限する。空・長大出力は切り捨てる。
- **融合はRRF。** exact順位と拡張順位の逆順位和で並べ替え、両方に無い行は落とさない。exact上位の順序を壊さない(first-stage exactを尊重)。
- **失敗は劣化。** 展開器の不在・タイムアウト・パース失敗は無言でexact-onlyに落ちる。`--status`相当の診断は`--verbose`/`stderr`に出し、stdout JSONはexact結果のまま壊さない。
- **キャッシュは任意・小。** 同一クエリの展開結果を小規模LRU(例64件・プロセス内のみ)で保持する。DB永続化はしない(本プランでは不要)。

---

## Task 1: `--expand`本体

**Files:** `src/cli.rs`、`src/search.rs`

- [ ] **Step 1: `Commands::Search`に`--expand`を追加。** `bool`、既定false。`--exact`との併用は許可(拡張語はOR側に足す。`search-and-fallback.md`の二段と整合)。
- [ ] **Step 2: 展開器の呼び出しを実装。** `CCS_EXPAND_CMD`(未設定時は`claude -p <固定プロンプト>`)をstdinにクエリを渡して実行し、タイムアウト(例5秒)・非0終了・空出力はexact-onlyに劣化。送信内容はクエリ文面のみ。
- [ ] **Step 3: RRF融合。** exact検索(既存経路)と拡張語検索(語を足したORクエリ)の2ランキングを`score = 1/(k+rank)`(k=60)で合算し、層順序(`search-and-fallback.md`の4層)を保ったまま並べ替える。
- [ ] **Step 4: テスト:**
  - 展開器不在→exact-onlyでexit 0、stdout JSONが壊れない
  - タイムアウト→同上劣化
  - 拡張語ヒットがexact 0件を救済する(モック展開器で固定語彙を返す)
  - exact上位の順序が拡張で逆転しない(RRFのfirst-stage尊重)
  - クエリ文面以外が展開器に渡らない(引数・stdinのアサート)

## Task 2: ドキュメントと運用

**Files:** `skills/conversation-search/SKILL.md`、`skills/conversation-search/REFERENCE.md`、`README.md`、`CHANGELOG.md`

- [ ] **Step 1: SKILL.md Level 2に`--expand`を追記。** 「言い換えが思いつかないときの広げ手段」として。Level 1の既定には入れない旨を明記。
- [ ] **Step 2: REFERENCE.mdに仕様。** `CCS_EXPAND_CMD`の形式(クエリをstdinに受け、カンマ区切り語彙をstdoutに)、タイムアウト、失敗時劣化、本文を送らないこと、ローカルモデル例(Ollama/LM Studio)。
- [ ] **Step 3: CHANGELOG Unreleasedに記載。** 既定オフ・新規依存なし・ベクトル永続化なしの3点を明記。

---

## Verification

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build --release
./target/release/ai-conversation-search search "ブラウザでMarkdown開くツール" --json 2>/dev/null | jq '.results | length'
CCS_EXPAND_CMD="tr ',' '\n'" ./target/release/ai-conversation-search search "ブラウザでMarkdown開くツール" --expand --json 2>/dev/null | jq '.results | length'
```

期待値: 1本目は少なく、2本目は展開語で救済されて同等以上。展開器不在でもexit 0かつJSON契約維持。

---

## Ripple（別プランへの波及）

- `search-and-fallback.md`: 拡張語はOR段に足す。AND段の純度は保つ。
- `bigram-short-terms.md`: 拡張語の2文字成分もbigram解析に通す。順序は「拡張→bigram解析」。
- 将来のsqlite-vec導入時は本プランのRRF融合をベクトル順位にも拡張する(本プランではやらない)。
