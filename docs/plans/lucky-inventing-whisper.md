# 検索に bm25 関連度ランキングを導入し、多語クエリを AND → OR にする

## Context

セッション検索が「探しているものに当たらない」という問題が報告された。実 DB（780,740 メッセージ）で計測した結果、原因は検索エンジン（SQLite FTS5 trigram）ではなく実装の 2 点だった。

1. **多語クエリが AND 固定** — `src/search.rs:629` と `src/search.rs:788` でスペース区切りの語を `" AND "` で連結している。実測では `"パッケージ" AND "更新" AND "ドキュメント"` が **0 件**、OR にすると 5,910 件。ノイズが多いのではなく何も返っていなかった。
2. **関連度ランキングが存在しない** — 検索系の `ORDER BY` は全て `m.timestamp DESC`（`src/search.rs:595, 611, 668, 821`）。`bm25()` は repo 全体で未使用。trigram tokenizer は語境界を持たないため部分一致した結果を新着順に並べているだけだった。

「アップグレード」単語 1 つでの上位 12 件の実測比較:

| | 現行（timestamp DESC） | bm25 順 |
|---|---|---|
| 中身 | 10/12 が claude-mem の `<observed_from_primary_session>` | **12/12 が実会話** |
| 文字数 | 6,783〜51,862 | 61〜878 |

observer メッセージは 1 件数万字あり、bm25 の文書長正規化だけで自動的に沈む（全体の 8.8%、68,918 件）。**bm25 の導入だけで観測ノイズ問題が実質解決する**ため、`--exclude-project` は今回スコープ外とする。実行時間は 44ms で問題なし。

期待する結果: 単語 1 つでも多語でも、関連度の高い実会話が上位に来ること。

## 前提（検証済み）

- **スキーマ移行は不要**。`bm25()` は FTS5 のクエリ時補助関数で、既存テーブル（`src/schema.rs:325-331`）のまま使える。`MIGRATIONS`（`src/schema.rs:14-65`、現在の最大 version 8）に追加しない。
- **bm25 は負値を返し、より小さい（負に大きい）ほど高関連**。`ORDER BY bm25(...) ASC`。符号の取り違えが最頻の事故なので専用テストで固定する。
- **二段階クエリ（two-phase）は維持する**。プランナのバグ回避のため（`src/search.rs:635-639` のコメント）。
- **現状 phase 2 は bm25 スコアを結合できない** — `SearchResultRow`（`src/search.rs:41-53`）に rowid がなく、phase 2 の SELECT も `m.rowid` を取っていない。ここが最大の障壁。

## 決定事項

| 論点 | 決定 |
|---|---|
| 並び順 | **bm25 をデフォルト**。`--sort=recent` で従来の新着順に戻せる |
| 多語 | **OR に変更**。bm25 とセットで初めて成立（OR 単独導入は悪化させるので必ず同時に出す） |
| `--exclude-project` | スコープ外 |
| LIKE フォールバック（3 文字未満） | AND + 新着順のまま。ランキング信号がない経路で OR にすると純粋なノイズになる |

## 実装

対象は主に `src/search.rs`、CLI フラグ追加で `src/cli.rs`。

### Step 1: rowid の配線（振る舞い変更なし）

`SearchResultRow`（`src/search.rs:41-53`）に rowid を追加する。`--json` 出力は CLI の契約なので `#[serde(skip)]` で不変に保つ。

```rust
pub struct SearchResultRow {
    #[serde(skip)]
    pub rowid: i64,
    pub message_uuid: String,
    // 以下不変
}
```

`from_row`（`src/search.rs:56-73`）に `rowid: row.get("message_rowid")?` を追加。`from_row` は厳格なので、これを供給する **5 箇所すべて**の SELECT に `m.rowid AS message_rowid` を足す:

- `src/search.rs:590`（空クエリ経路）
- `src/search.rs:601`（LIKE 短語経路）
- `src/search.rs:659`（FTS phase 2 / `search_conversations`）
- `src/search.rs:733`（grouped の窓関数経路 — 内側 CTE に入れれば外側 `SELECT *` が伝播する）
- `src/search.rs:809`（FTS phase 2 / grouped）

`unwrap_or(0)` で握り潰さないこと。1 箇所漏らすと全行 rowid=0 になりスコア引きが全滅する。この step 単体で `cargo test --all-targets` が通ること（通らなければ SELECT 漏れ）。

### Step 2: phase 1 がスコアを返す

`query_fts_rowids`（`src/search.rs:911-919`）を差し替える。

```rust
/// (rowid, bm25) を高関連順で返す。bm25 は負値で、小さいほど高関連。
fn query_fts_rowids(&self, fts_query: &str) -> Result<Vec<(i64, f64)>> {
    let mut stmt = self.conn.prepare(
        "SELECT rowid, bm25(message_content_fts) AS score \
         FROM message_content_fts WHERE full_content MATCH ? \
         ORDER BY score",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![fts_query], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<(i64, f64)>, _>>()?;
    Ok(rows)
}
```

FTS 破損時の自動 rebuild 経路（`src/search.rs:933-935`、`rebuild_fts` は `src/search.rs:1536`）はそのまま維持する。呼び出し 2 箇所（`src/search.rs:640, 794`）を `chunk.iter().map(|(r, _)| ...)` に追従。この時点ではまだ timestamp でソートし、テストは緑のまま。

### Step 3: `--sort` フラグ

`Commands::Search`（`src/cli.rs:121-164`）に追加する。

```rust
/// 並び順（relevance = bm25, recent = 新着順）
#[arg(long, value_parser = ["relevance", "recent"], default_value = "relevance")]
sort: String,
```

`SearchFilter`（`src/search.rs:13-22`）に `sort: SortOrder` を追加し、`Default` は `SortOrder::Relevance`。`SearchFilter` 構築箇所は `src/cli.rs:353-362`（search）と `src/cli.rs:388-397`（list）。**list は既存どおり `Recent` を明示**する — list は関連度の概念がない。

`SortOrder` は `src/search.rs` に定義する。`filter.sort == SortOrder::Relevance` の比較のため `PartialEq` が必須:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Relevance,
    Recent,
}
```

CLI の `String` → `SortOrder` 変換が要る。`Commands::Search` の分解（`src/cli.rs:331-345`）に `sort` を追加し、`match sort.as_str()` で変換する（`value_parser` が値を保証しているので他は `unreachable!`）。あるいは `SortOrder` に `clap::ValueEnum` を derive してもよい。

**`cmd_search` / `cmd_search_grouped`（`src/cli.rs:675-690, 785-791`）のシグネチャ変更は不要** — 既に `&SearchFilter` を受け取っているため、余計な引数を通さないこと。

### Step 4: `search_conversations` を bm25 順に

`src/search.rs:640-682` を差し替える。`ORDER BY m.timestamp DESC`（`src/search.rs:668`）は Rust 側で並べるため削除。

```rust
let scored = self.query_fts_rowids(&fts_query)?;   // 既に高関連順
if scored.is_empty() { /* 既存の早期 return（src/search.rs:642-648）のまま */ }

let score_by_rowid: HashMap<i64, f64> = scored.iter().copied().collect();

const BATCH_SIZE: usize = 500;
let mut all_results: Vec<SearchResultRow> = Vec::new();

for chunk in scored.chunks(BATCH_SIZE) {
    // ... プレースホルダ生成は既存どおり、rowid は chunk の .0 から
    Self::append_filters(&mut sql, &mut batch_params, filter)?;
    all_results.extend(self.execute_search_typed(&sql, &batch_params)?);

    // 早期打ち切りは relevance 順のときだけ成立する（下記の不変条件を参照）
    if filter.sort == SortOrder::Relevance && all_results.len() >= limit as usize {
        break;
    }
}

match filter.sort {
    SortOrder::Relevance => all_results.sort_by(|a, b| {
        let sa = score_by_rowid.get(&a.rowid).copied().unwrap_or(f64::MAX);
        let sb = score_by_rowid.get(&b.rowid).copied().unwrap_or(f64::MAX);
        sa.partial_cmp(&sb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.timestamp.cmp(&a.timestamp))   // 同点は新しい順
    }),
    SortOrder::Recent => all_results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp)),
}
all_results.truncate(limit as usize);
```

`total_cmp` ではなく `partial_cmp(...).unwrap_or(Equal)` を使う（NaN 安全かつ clippy 対応）。

#### 早期打ち切りの不変条件（コードにそのままコメントとして残す）

**やってはいけないこと**: phase 1 の rowid 列を `limit` 件に切り詰めてから phase 2 に渡す。フィルタ（project/date/source/repo、`append_filters` は `src/search.rs:867-909`）と `is_meta_conversation = FALSE` は **phase 2 でしか適用されない**。上位 20 件が全て project X で `--project Y` が指定されていた場合、正解が非空なのに 0 件を返す。

**成立する最適化**: phase 1 が昇順（高関連順）で返すので、バッチ `0..k` を処理した時点で、未処理の候補はすべて既収集行以上のスコア（＝同等以下の関連度）を持つ。したがって `all_results.len() >= limit` になった時点で、手元の上位 `limit` 件はフィルタ後の全体でも上位 `limit` 件である。フィルタは候補を減らすだけで順位を上げないので、この不変条件はフィルタの有無に依存しない。

5,910 件ヒットで limit=20 なら、12 バッチではなく通常 1 バッチで済む。

**制約**:
- `SortOrder::Recent` では成立しない（新しさと bm25 順は無相関）ため、上記のとおりガードする。
- バッチ境界での同点は順序が不定。trigram のスコアで完全一致は稀だが起こりうる。許容する。
- `limit == 0` は即 break して空になる。正しいが、`search_conversations` は grouped（`src/search.rs:716-721`）と違い limit 検証がないのでテストで固定する。
- `stats.matched_messages` は現状すでに truncate 後に計算されている（`src/search.rs:683`）。この PR では意味論を変えない。

### Step 5: grouped を bm25 順に

セッションのスコア = そのセッションの**最良（最小）bm25**。代表メッセージ = 最新ではなく**最良スコアのメッセージ**。

代表を動かす理由: あるセッションが「非常に関連の高いメッセージを含むから」上位に出たのに、スニペットとして別の（単に最新の）メッセージを見せると、ランキングが壊れて見える。

grouped 側にも **`score_by_rowid` の構築と `match filter.sort` の分岐が必要**（Step 4 のものは `search_conversations` のローカルスコープにあるため共有されない）。これを省くと `--sort=recent --group-by-session` がフラグを黙って無視する。

```rust
let score_by_rowid: HashMap<i64, f64> = scored.iter().copied().collect();

match filter.sort {
    SortOrder::Relevance => all_results.sort_by(|a, b| {
        let sa = score_by_rowid.get(&a.rowid).copied().unwrap_or(f64::MAX);
        let sb = score_by_rowid.get(&b.rowid).copied().unwrap_or(f64::MAX);
        sa.partial_cmp(&sb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.timestamp.cmp(&a.timestamp))
    }),
    SortOrder::Recent => all_results.sort_by(|a, b| b.timestamp.cmp(&a.timestamp)),
}
```

構造の変更はこれだけでよい。`all_results` が高関連順に並べば、既存のループ（`src/search.rs:836-848`）が作る `order` はそのままセッションスコア順になり、最初に見た行が最良スコア行になる。`src/search.rs:821` の `ORDER BY m.timestamp DESC` は削除。`match_count` の意味論は不変。

`SortOrder::Recent` のときは代表メッセージも従来どおり最新行になる（ソート結果がそのままそうなる）。

**grouped には早期打ち切りを入れない**。`match_count`（`src/search.rs:845`）と `total_matched_messages`（`src/search.rs:828`）が全件走査を要求するため。後で誰かが「最適化」してカウントを壊さないよう、その旨をコメントで明示する。

### Step 6: AND → OR

`src/search.rs:629` と `src/search.rs:788` の `.join(" AND ")` を `.join(" OR ")` に変更。これだけ。

サニタイザの他の部分は不変 — 単語 1 つは `format!("\"{}\"", terms[0])` の分岐のまま（`src/search.rs:622-623`）、`AND`/`OR`/`NOT`/`"` を含むクエリは素通し（`src/search.rs:616-620, 775-778`）。したがって `--exact`（`src/cli.rs:347-352`）は phrase クエリを作るので join を経由せず無影響。

同時に LIKE 経路（`src/search.rs:598-613, 727-771`）へ「AND + 新着順のままにしている理由」をコメントで残す。2 文字の語を OR にするとほぼ全件に当たり、ランキング信号がないので救えないため。

`test_query_sanitization`（`src/search.rs:3032`、assertion は `3062-3068`）は「Multi terms — AND join」というコメントを持つが、フィクスチャにメッセージが 1 件しかないため OR にしても偶然通ってしまう。**コメントと期待値を更新する** — 放置すると偽の回帰ガードになる。

### `pick`（fzf ピッカー）への影響

`bin/ai-conversation-search:216-222` の reload スクリプトは `search "$q" --group-by-session --json` をキーストロークごとに呼んでおり、`--sort` を渡していない。**意図的に渡さない**ことで新デフォルト（relevance）を継承させる。

- インクリメンタル検索の UI では関連度順の方が適切
- ラッパーはバージョン pin されている（`bin/ai-conversation-search:10-12`）ため、`--sort` を無条件に渡すと古いキャッシュ済みバイナリで壊れる。渡さなければこの問題が発生しない

ただし表示行は `last_message_at` と `match_count` を出している（`bin/ai-conversation-search:230-231`）ため、**並び順と表示されている日付が一致しなくなる**。ラッパー側の変更は今回行わないが、この不一致は認識した上での判断であることを記録しておく。表示に関連度を出すかどうかは別途判断する。

### Step 7: 実 DB での検証（Q6: 短文バイアス）

**v1 ではガードを入れない**。bm25 の文書長正規化は今回の効果そのもの（observer メッセージを沈める）であり、未計測の失敗モードに対して先回りでチューニングしない。

ただし監視すべき失敗モードがある: 15 字の `"OAuth"` だけのメッセージが、2,000 字の OAuth 設計議論を上回る可能性。実測では OR 併用時に 38 字のメッセージが 1 位に来ている。

検証手順: 実 DB に対し代表的なクエリを数本流し、上位 20 件のうち 100 字未満が何件あるか数える。汚染が実在したら、フィルタではなくソートキーの同点処理で対応する（`(bm25, -len)`）。ハードな長さ下限は「短いが正しい」メッセージを検索不能にするので採らない。Rust 側でソートしているのはこの調整を 1 行で入れられるようにするため。この判断はコメントに残す（見落としではなく検討済みであると分かるように）。

## テスト

`setup_test_db()`（`src/search.rs:1570`）、`insert_test_message`（`src/search.rs:1579`）、`from_connection`（`src/search.rs:1549`）で全て賄える。新しいハーネスは不要。

### 変更が必要な既存テスト

| テスト | 位置 | 新しい期待値 |
|---|---|---|
| `test_search_multi_term_and_join` → `..._or_join_ranked` にリネーム | `src/search.rs:3292` | 2 行返り、両語を含む msg1 が 1 位 |
| CJK 多語テスト | `src/search.rs:3115` 付近 | 2 行、msg1 が 1 位 |
| `test_search_grouped_representative_is_most_recent` → `..._is_best_scoring` | `src/search.rs:2451` | 代表が高スコア側。`match_count == 2` は不変。**フィクスチャの修正が必須** — 現在の 2 件はスコアがほぼ同点で、float の偶然でテストが通ってしまう。片方を短い密な一致、もう片方を語を含む 2,000 字の埋め草にする |
| `test_search_snippet_trigram` | `src/search.rs:3334` | 期待値は不変だが OR になるので再実行して確認 |

### 追加するテスト

- **T-N1 `test_bm25_sign_convention_lower_is_better`** — 短く密な一致と長く疎な一致。前者が `results[0]`。`ORDER BY` の符号反転を検出する
- **T-N2 `test_bm25_ranking_beats_recency`** — 古くて高関連 vs 新しくて低関連。古い方が 1 位。この機能の存在理由そのもので、他に固定しているものがない
- **T-N3 `test_rowid_join_survives_filters`** — 2 プロジェクト 3 セッション、`project_path: Some("/projA")` で検索。projA のみ、かつスコア順。`append_filters` が行を落とした後の `score_by_rowid` 引きを守る
- **T-N4 `test_early_stop_respects_filters`** — **最重要**。600 件ほど投入してバッチ処理を実際に発生させ、第 1 バッチが全て `--project` で落ち、生存行が後続バッチにある構成で非空を assert する。Step 4 の素朴な切り詰めバグに対する唯一の防波堤
- **T-N5 `test_search_zero_limit_returns_empty`** — FTS 経路で `limit: 0` が panic せず 0 件
- **T-N6 `test_grouped_session_score_is_best_message`** — セッション A（1 件優秀＋多数凡庸）が B（全て凡庸）より先。A の代表は優秀な方。min を採った選択を固定する
- **T-N7 `test_grouped_match_count_unaffected_by_ranking`** — 3 件一致のセッションが `match_count == 3` のまま。grouped への早期打ち切り混入を防ぐ
- **T-N8 `test_like_fallback_still_and_and_recency_ordered`** — 2 文字クエリで AND 意味論と新着順。意図的な非対称性を固定する
- **T-N9 `test_json_output_has_no_rowid_field`** — `SearchResultRow` を serialize して `rowid` を含まないこと。`#[serde(skip)]` を守る
- **T-N10 `test_sort_recent_restores_timestamp_order`** — `--sort=recent` 相当で従来の新着順に戻ること。**grouped 経路でも同じ assert を置く**（Critical 指摘: grouped の分岐漏れを検出する唯一のテスト）

`limit` の境界について: `search_grouped_by_session` は負値を検証する（`src/search.rs:716-721`）が `search_conversations` はしない。負値では `limit as usize` が巨大値になり早期打ち切りが発火せず truncate も no-op になる。これは現行と同じ挙動なので回帰ではないが、T-N5 を `limit: -1` にも拡張しておく。

## ドキュメントとリリース

- `skills/conversation-search/REFERENCE.md:53`（`search` のフラグ一覧）と `:68`（オプション説明）に `--sort` を追加し、デフォルトが関連度順になったことを明記する
- `CHANGELOG.md` に破壊的でない挙動変更として記載する（デフォルトの並び順変更と多語の OR 化は、ユーザから見える挙動変更）
- リリースは `CLAUDE.md` の手順に従う（`./scripts/bump-version.sh <new-version>` → タグ push）

## 検証

各 step は単独でコンパイルが通り、テストが緑であること（Step 6 は既存テストの期待値更新を伴う）。

```bash
cargo test --all-targets --all-features
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

実 DB でのエンドツーエンド確認（`~/.conversation-search/index.db`、780k メッセージ）:

```bash
cargo build --release

# 1. 単語1つ: observer ノイズが消え実会話が上位に来ること
./target/release/ai-conversation-search search "アップグレード" --limit 20

# 2. 多語: 従来 0 件だったものがヒットすること
./target/release/ai-conversation-search search "パッケージ 更新 ドキュメント" --limit 20

# 3. 従来の挙動に戻せること
./target/release/ai-conversation-search search "アップグレード" --sort=recent --limit 20

# 4. grouped で代表メッセージが最良スコア行になっていること
./target/release/ai-conversation-search search "アップグレード" --group-by-session --limit 10

# 5. --json 出力に rowid が現れないこと
./target/release/ai-conversation-search search "アップグレード" --limit 1 --json | grep -c rowid   # → 0

# 6. grouped でも --sort=recent が効くこと（レビュー指摘の分岐漏れ確認）
./target/release/ai-conversation-search search "アップグレード" --group-by-session --sort=recent --limit 10

# 7. pick が新デフォルト（関連度順）で動くこと
./target/release/ai-conversation-search pick

# 8. Step 7 の短文バイアス確認: 上位20件の文字数分布を目視
```

期待値: (1) は上位が実会話中心（計測時の bm25 順を再現）、(2) は 5,910 件ヒットの上位に多語一致が来る、(3) は現行 main と同じ並び。

## スコープ外（別 PR）

- `--exclude-project`（observer 除外）。bm25 で大半が片付くため効果を確認してから判断する。実装する場合、パス表記が 2 種類あるため完全一致では取りこぼす（`~/.claude-mem/observer-sessions` と、パス変換が壊れた `/Users/yoshiki/kadono//claude/mem/observer/sessions`）。前方一致が必要
- `stats.matched_messages` の意味論。`search_conversations` の FTS 経路は truncate 後（`src/search.rs:683`）、grouped は truncate 前（`src/search.rs:828`）で不整合。ランキングの差分と混ざるので分離する
- `SKILL.md` の検索手順への反映（1 語 + `--group-by-session` 等の運用知識）
