# 短語・CJK救済 — bigram解析で2文字クエリをFTSに届かせる

> **For agentic workers:** TDD で1タスクずつ。各ステップは `- [ ]`。

**Goal:** `mo`のような2文字クエリと日本語2文字クエリ(`失敗`、`検索`等)をFTSランキングに載せる。現状は`MIN_TRIGRAM_CHARS=3`未満がLIKE全走査(AND・非ランク・新着順)に落ち、最も弱い経路を通る。アプリ層のbigram解析(参考: `yaalsn/ccsearch`、 `streetwriters/sqlite-better-trigram`)で救済する。

**Non-goal:** ベクトル検索、クエリ拡張(`query-expansion.md`)、AND/OR意味論の変更(`search-and-fallback.md`)。SQLite拡張(.so)の同梱はしない。porter/stemmingの導入なし。

**Architecture:** Rustアプリ層の解析のみ。SQLite拡張も外部クレートも増やさない。`tokenize='unicode61'`の別FTS表(`message_content_bigram_fts`)を新設し、既存trigram表(`message_content_fts`)と併用する。インデックス時・クエリ時に同一のRust解析を通す。ランキングは単一表のbm25のみを使い、二表のスコア加算はしない(詳細は設計判断)。

---

## Context

- `MIN_TRIGRAM_CHARS=3`(`src/search.rs:266`)。`plan_query`(`src/search.rs:283-327`)の仕様は: 全語が2文字以下なら`LikeOnly`(全表LIKE・AND・新着順、`src/search.rs:298-300,709-737`)、3文字以上の語が1つでもあれば`Hybrid`(長語をFTSのOR連結にし、短語はphase-2のLIKE減算、`src/search.rs:319-327,784-793`)。`mo`単独は前者、`デプロイ 失敗`は後者に落ちる。FTSの`MATCH`は3文字未満の部分文字列にヒットしない(SQLite仕様、`sqlite.org/fts5.html#trigramidx`)。
- 現行スキーマは`message_content_fts(message_uuid UNINDEXED, full_content, tokenize='trigram ...')`(`data/schema.sql:50-56`)。現行のLIKE経路はすべて`%term% ESCAPE '\'`形式(`src/search.rs:720-721,791-792`)のため、trigram表のLIKE最適化の対象外(全表走査)である。
- `yaalsn/ccsearch`方式: インデックス時とクエリ時に同一解析を通す。CJKラン→重なりbigram(`全文检索`→`全文 文检 检索`)、ASCIIは単語のまま+prefix化。これで2文字クエリが当たる。
- `messages`→FTS同期はトリガ(`messages_ai/ad/au`)で行う(`data/schema.sql:59-80`)。新表を足したらトリガとマイグレーションの更新が必須。既存DBの孤児エントリ事故(v0.15.0修正、`CHANGELOG.md:123`、`src/schema.rs:358-371`)の再発防止のため、`'delete'`コマンド形式を維持する。
- 書き込み経路は3箇所で意味論が異なる: `src/indexer/claude_code.rs:1433`(`INSERT OR IGNORE`＝既存行スキップでトリガ不発火)、`src/indexer/codex.rs:421`(`INSERT OR REPLACE`)、`src/indexer/opencode.rs:267`(`INSERT OR REPLACE`)。Claude側の`do_index_conversation`(`src/indexer/claude_code.rs:1182-1509`)は既存セッションでは未登録UUID差分のみ挿入する(`src/indexer/claude_code.rs:1340-1369`)。
- 再インデックスなしで読めるのは必須。`index --all --force`はmtimeスキップを外すだけでsecond-levelスキップ(既存UUID差分のみ挿入)は残るため、既存行の書き換えには使えない(`CHANGELOG.md:[0.16.1]`、`CHANGELOG.md:53-55,88-93`参照)。bigram表のバックフィルはトリガ発火UPDATEまたは`rebuild`ベースで行う(設計判断参照)。

## 設計判断

- **SQLite拡張は同梱しない。** 単一バイナリ・`bundled`維持(`Cargo.toml:13`)を優先し、Rust側の前処理+通常FTS表で実現する。配布とCIの複雑化を避ける。
- **FTS5の`tokenize`はテーブル単位のため、別表新設の一択とする。** 既存表(`tokenize='trigram'`)への列追加案は採用しない。trigramトークナイザは2文字トークンを捨てるため、同じ表に空白区切りbigramを入れても索引されない(`src/search.rs:5424-5434`のCJK 2文字不ヒットが証拠)。新表は以下とする(standalone、`content=`なし。理由はトリガ項参照):
  ```sql
  CREATE VIRTUAL TABLE IF NOT EXISTS message_content_bigram_fts USING fts5(
      message_uuid UNINDEXED,
      bigram_content,
      tokenize='unicode61 remove_diacritics 1'
  );
  ```
  アプリ層でCJKランを空白区切りbigram化しておき、`unicode61`には単語トークンとして食わせる(`ccsearch`と同型)。オプション記法はスペース区切り(`remove_diacritics 1`)。`=`形式(`remove_diacritics=1`)はbundled SQLite 3.51.1で`parse error in tokenize directive`になるため使わない。
- **同期はトリガ＋Rustスカラー関数で行い、indexerは変更しない。** トリガ(SQLのみ)からRust解析を呼ぶ手段として、`bigram_analyze(text)`を`rusqlite`の`create_scalar_function`で登録する(`src/bigram.rs:register_sql_function`。`functions`フィーチャ追加。拡張`.so`ではなくコンパイルフラグのためNon-goalに抵触しない)。`messages_ai/au`は`bigram_analyze(new.full_content)`を書き込むため、3 indexerのINSERT文(意味論は`INSERT OR IGNORE`/`REPLACE`で異なる)に手を入れず全経路を救済できる。登録先は`db::connect`と`schema::init_schema`の両方(テスト用インメモリ接続を含む)。
- **bigram表はstandaloneのため、削除は素の`DELETE`で行う。** trigram表(外部コンテンツ表)が`'delete'`コマンド必須なのと逆で、standalone表への`'delete'`コマンドは`SQL logic error`になる(実測)。両者を「統一」しないこと。`data/schema.sql`のコメントに理由を記載。
- **CJK bigram + ASCII prefixの二系。** 日本語・中国語・韓国語のランだけbigram化し、ASCIIは`unicode61`相当の単語分割+prefix(`foo*`)で扱う。ASCIIまでbigram化すると`crit`→`cr ri it`で誤爆が増えるため。
- **既存trigram表は残す。ランキング融合はしない(フィルタ分担)。** 3文字以上の部分文字列検索の精度を維持する。二表のbm25加算は尺度が合わないため行わない。役割分担は:
  - 長語(3文字以上)ありの混合クエリ: phase-1はtrigram表で`query_fts_rowids`(`src/search.rs:1270-1282`)し、bigram条件はrowid集合の絞り込み(ANDフィルタ)として使う。最終順序はtrigram側bm25のみで決める。
  - 全語短語(2文字以下)のクエリ: phase-1はbigram表に対して`bm25(message_content_bigram_fts)`で順序付けする。trigram表は使わない。
  - どちらでも拾えない残差(1文字語・絵文字・記号のみ等)は従来どおりphase-2のLIKEに残す。
  - `crit`(4文字)のtrigram部分一致(`critical`/`alacritty`)はbigram側ANDの足切りを受けない(短語を含まないクエリはbigram表に触れない)。
- **バックフィルは`index --all --force`を使わない。** 上記Contextのとおり`--force`では既存行が書き換わらない。手段は (a) `UPDATE messages SET full_content = full_content WHERE ...`で`messages_au`トリガ(`data/schema.sql:75-80`)を発火させるSQL手順(`CHANGELOG.md:88-93`の`[Task notification]`遡及と同型)、または (b) `rebuild`コマンド(`src/search.rs:2025-2033`)＋`rebuild_fts`(`src/indexer/claude_code.rs:1580-`)の拡張、のいずれか。マイグレーション内で全行UPDATEはしない(バックグラウンドインデクサ内で重い処理を走らせない。`CHANGELOG.md:131`の`prune-observer`分離と同理由)。バックフィルは冪等な手動コマンド(ドライラン付き)として提供する。
- **`search-and-fallback.md`との順序。** 本プランは`search-and-fallback.md`のAND→OR二段の上に載せる。bigram絞り込みはAND段・OR段の両方に付随させるが、二段の枠組み自体は変えない。先に`search-and-fallback.md`を実施した場合は本プランTask 3で両段への付随をテストし、先に本プランを実施した場合は`search-and-fallback.md`側でOR段のbigram付随を壊さないこと。

---

## Task 1: bigram解析器

**Files:** `src/` 新規モジュール(例 `src/bigram.rs`)、既存 `src/search.rs` から利用

CJK範囲定義(固定): ひらがな(U+3041-3096)、カタカナ(U+30A1-30FF)、漢字(U+3400-4DBF, U+4E00-9FFF, U+F900-FAFF, U+20000-2EBEF)、ハングル(U+AC00-D7AF, U+1100-11FF, U+3130-318F)。全角ASCII(U+FF00-FFEF)はASCIIに正規化して扱い、半角カナ・句読点・絵文字・制御文字・記号はランの区切りとする(単独ではbigram化しない→LIKE残差)。

- [x] **Step 1: `analyze_for_index(text) -> String`を実装。** CJKランを重なりbigram化、ASCIIは単語のまま空白区切りで出す。例: `全文检索`→`全文 文检 检索`、`k1LoW/mo markdown`→`k1LoW mo markdown`(2文字も落とさない)。1文字だけのCJKランは出力しない(検索時にLIKE残差になる)。
- [x] **Step 2: `analyze_for_query(term) -> String`を実装。** 規則を以下に固定する:
  - 2文字CJK→引用1語(`"失敗"`)、3文字以上CJK→引用bigramのAND(`全文检索`→`"全文" AND "文检" AND "检索"`。phraseではなくAND。順序制約を付けない)。
  - ASCII 2文字以下→引用1語(`"mo"`)、ASCII 3文字以上→素のprefix(`markdown*`。`"markdown"*`の形にしない。FTS5 syntax errorになるため)。
  - FTS5特殊文字(`"`は doubling、`*`、`AND/OR/NOT`混入)はエスケープする。`Raw`経路(引用符含み、`src/search.rs:285-287`)と`title_terms`スキップ規則(`src/search.rs:1121-1130`)は本プランで変更しない。
- [x] **Step 3: `bundled` FTS5機能の確認テスト。** `unicode61`トークナイザと`trigram`が有効なことを`COMPILE_OPTION`または実MATCHでアサートする。
- [x] **Step 4: テスト:**
  - `mo`が解析落ちしない(空にならない)
  - `失敗`→`失敗`1語、`全文检索`→3語のbigram列
  - ASCII短語とCJKの混在クエリ(`k1LoW mo markdown`)の展開が期待通り
  - 1文字(`あ`/`a`)・空文字・絵文字・制御文字・全角ASCII・半角カナでパニックなし、かつ1文字語はbigram語を出さない(LIKE残差になる)
  - `"`/`*`を含む語のエスケープ(`"a\"b"`がsyntax errorにならない)

## Task 2: スキーマと同期

**Files:** `data/schema.sql`、`src/schema.rs`(マイグレーション: version 10追加)、`src/indexer/claude_code.rs`、`src/indexer/codex.rs`、`src/indexer/opencode.rs`

- [x] **Step 1: `message_content_bigram_fts`の新設(別表のみ。既存表への列追加はしない)。** 上記設計判断のDDLを用いる。`messages_ai/ad/au`の3トリガ本体を拡張し、1回のINSERT/UPDATE/DELETEで両FTS表に書き込む(trigram側は`'delete'`形式、bigram側は素の`DELETE`。理由は設計判断参照)。`FTS_SYNC_TRIGGERS`(`src/schema.rs:13-29`)と`data/schema.sql`の二重管理を更新する。
- [x] **Step 2: 書き込み経路の更新。** スカラー関数方式のためindexer 3箇所のINSERT文は無変更(`src/indexer/claude_code.rs:1433`、`src/indexer/codex.rs:421`、`src/indexer/opencode.rs:267`)。トリガが`bigram_analyze`で算出するため「表はあるが誰も書かない」状態にならない。各経路の`INSERT OR IGNORE`/`REPLACE`差はトリガ発火差として吸収される(IGNOREスキップ行は未変更のため何も書かないのが正しい)。
- [x] **Step 3: マイグレーションversion 10。** 新規DBは`schema.sql`で完結、既存DBはversion 10で新表＋トリガ置換を行う。`detect_custom_migration_applied`(`src/schema.rs:249-272`)にversion 10分岐(`messages_ai`に`bigram`を含むか)を追加し、`bootstrap_existing_db`(`src/schema.rs:174-201`)で検出できるようにする。マイグレーション内での全行UPDATEはしない(設計判断参照)。
- [x] **Step 4: バックフィル手順(手動コマンド)。** `backfill-bigram`サブコマンド(`--dry-run`付き)として提供する。`count_bigram_missing`/`fill_bigram_missing`(`src/schema.rs`)の単文INSERTで冪等・再実行可。`index --all --force`での埋め直しは手段にしない(上記Context参照)。手順をREFERENCEに記載する。
- [x] **Step 5: テスト:**
  - INSERT→両FTS表に反映、DELETE→両表から消える(孤児なし。`src/schema.rs:446-475`の`orphan_fts_rows`流用をbigram表にも)
  - UPDATE→旧語が両表から消える(v0.15.0の回帰テスト`src/schema.rs:477-506`相当をbigram表にも)
  - `FTS_SYNC_TRIGGERS`と`data/schema.sql`の一致テスト更新(`src/schema.rs:511-545`)
  - fresh DB / legacy DB / partial DB(`src/schema.rs:687-762`相当)でマイグレーションが緑、version連番テスト(`src/schema.rs:780-790`)更新

## Task 3: 検索経路の接続

**Files:** `src/search.rs`

- [x] **Step 1: `plan_query`(`src/search.rs:283-327`)の短語分岐を更新。** 2文字語もbigram FTSに届く場合はFTS側に含め、LIKE減算はbigramで拾えない残差(1文字語・絵文字・記号のみ等)のみに縮小する。新variant `BigramOnly`(全語短語だがbigram到達可能)、`Hybrid`に`bigram_query`(AND結合・phase-2のrowid絞り込み)を追加。`LikeOnly`は「どのFTSにも届かない語のみ」に縮小。`Raw`・`--exact`・明示演算子・`title_terms`の扱いは変えない(短語オペランド付き演算子はLIKE literalのまま)。
- [x] **Step 2: `search_conversations`/`search_grouped_by_session`の二段と整合。** 設計判断のフィルタ分担(混合→trigram bm25＋bigram絞り込み、全語短語→bigram bm25)で実装する。`query_bigram_rowids`を追加し、二表のスコア加算はしない(`FtsTable`で切替)。`inject_title_matches`は両段の後に1回のみ。groupedの全バッチ走査不変条件を維持し、`--sort recent`の挙動は変えない。スニペットはbigram展開語でハイライトがずれないこと。
- [x] **Step 3: テスト:**
  - `mo`単独クエリがLIKE全走査ではなくbigram FTS経路でbm25順に返る(新着順でないことをアサート)
  - `失敗`単独(2文字)が0件にならない
  - 混合クエリ(`デプロイ 失敗`)でtrigram順序が保たれ、短語のLIKE減算がbigram絞り込みに置き換わる
  - `crit`(4文字)のtrigram挙動が劣化しない(`critical`/`alacritty`への部分一致は残る。消すのは本プランの責務ではない)
  - 単語1つ・`--exact`・明示`AND`/`OR`/`NOT`・引用符は従来通り
  - 実DB規模での実行時間: 短語クエリは全表LIKEより速く、通常(長語のみ)クエリは二表化前から退行しないこと(`candidates_scanned`と`truncated`過大報告`src/search.rs:855`に注意)

---

## Verification

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build --release
./target/release/ai-conversation-search search "mo" --group-by-session --limit 20 --json 2>/dev/null | jq '.results | length'
./target/release/ai-conversation-search search "失敗" --group-by-session --limit 20 --json 2>/dev/null | jq '.results | length'
```

期待値: 両方とも0件ではなく、かつ`truncated`含めenvelope契約が維持される。`mo`が新着順ではなくbm25順で返ること(代表メッセージがLIKE時代と変わる)。`mo`で全表LIKE遅延が出ないこと。通常の長語クエリ(`crit`等)の結果件数・順序が二表化前と大きく変わらないこと。

---

## Ripple（別プランへの波及）

- `search-and-fallback.md`: 短語のLIKE減算の縮小は本プラン実施後に見直す。AND/OR二段の枠組み自体は維持。本プランのbigram絞り込みはAND段・OR段の両方に付随させる。実施順序によらず相手側の段を壊さないこと。
- `query-expansion.md`: 拡張語の2文字成分もbigram経路に載る。前処理の順序は「拡張→bigram解析」とする。
