# 短語を含むクエリのハイブリッド化（FTS + LIKE 絞り込み）

## Context

前 PR（b288595）で FTS 経路に bm25 ランキングと OR 結合を入れた。しかし
`src/search.rs:602` / `:790` の `has_short_term` により、**3 文字未満の語が 1 つでも
あるとクエリ全体が FTS を迂回**し、全表 LIKE スキャン（AND + 新着順）に落ちる。
日本語では 2 文字語が頻出するため、この経路に落ちるクエリは少なくない。

実 DB（780,740 メッセージ）で計測した結果:

| クエリ `デプロイ 失敗`（`失敗` が 2 文字） | 上位 6 件の中身 | 文字数 |
|---|---|---|
| 現状（全表 LIKE + AND + 新着順） | **6/6 が observer transcript** | 11,444〜91,464 |
| ハイブリッド（FTS `デプロイ` + bm25、`失敗` は LIKE 絞り込み） | **6/6 が実会話** | 99〜1,307 |

性能も **7.596s → 0.125s（約 60 倍）**、返る件数は 679 で一致。
trigram tokenizer は 3 文字以上の語では部分一致そのものなので、FTS は LIKE の
完全な代替である（実測: `LIKE '%Docker%'` = 4,636 と `MATCH '"Docker"'` = 4,636、
`LIKE '%リファクタ%'` = 2,745 と FTS = 2,745）。つまりヒット集合を変えずに
ランキングと速度だけを得られる。

期待する結果: 3 文字以上の語を 1 つでも含むクエリなら、短語が混ざっていても
bm25 ランキングと FTS の速度を享受できること。

## 前提（実測で確認済み）

- **3 文字未満のトークンを FTS に渡してもエラーにならず、単に何にもマッチしない**。
  実測: `MATCH '"更新"'` → 0 件、`MATCH '"Docker" OR "更新"'` → 4,652 件（= Docker 単独と同じ）。
  よって短語は FTS に渡さず、phase 2 の LIKE で絞り込む必要がある。
- **既存バグ: `OR` 演算子クエリが FTS に届いていない**。`has_short_term` の判定
  （`src/search.rs:602`）はトークン長だけを見るため、`OR` が 2 文字であることから
  `Docker OR リファクタ` が短語クエリと誤判定され、`%OR%` を含む LIKE-AND になる。
  実測: FTS で 7,264 件のところ CLI は 136 件しか返さない（`%Docker%` AND `%OR%`
  AND `%リファクタ%` = 137 件と一致）。`AND` / `NOT` は 3 文字なので影響なし。
  今回の分岐再構成でこのバグは自然に解消する。

## 設計

語ごとに「使える最良のシグナル」を割り当てる、という一つの規則にまとめる。

| 語 | 扱い |
|---|---|
| 3 文字以上 | FTS で照合。OR 結合、bm25 でランキング |
| 3 文字未満 | phase 2 の `LIKE` で**必須の絞り込み条件**として適用（ランキングには寄与しない） |
| 3 文字以上の語が 1 つも無い | 従来どおり全表 LIKE + AND + 新着順（変更なし） |

短語を必須条件にする理由: ランキング信号を持たない語を OR に混ぜると、その語は
実質無視される（3 文字未満のトークンは FTS で何にもマッチしない）。順位付けできない
語は「絞る」ことにしか使えない。逆に長語を AND にしないのは前 PR の決定
（OR + bm25）を維持するため。

**却下した代案: 短語を完全に捨てる**（`デプロイ 失敗` → `デプロイ` だけで検索）。
分岐が減り性能上も有利だが、実測すると意図が失われる。`"デプロイ"` 単独の bm25 上位
6 件は**全件が `失敗` を含まない**（デプロイ手順・成功報告の会話）。ユーザーが
`失敗` と書いた意図が完全に消えるため採らない。

分岐の判定順も入れ替える。ただしレビュー指摘のとおり「演算子パススルーを単純に
先頭へ」は退行を生む — `更新 OR 認証` のように**全語が短いクエリ**が生のまま FTS に
行くと 0 件になる（今日は不格好ながら行が返る）。全語短のチェックを演算子判定より
先に置く。

```
1. クエリが空                          → 従来どおり（新着順）
2. " を含む                            → 生のまま FTS へ（引用符は既存どおり最優先）
3. 3 文字以上の語が 0 個               → 従来どおり全表 LIKE + AND + 新着順
4. " AND " / " OR " / " NOT " を含む   → 生のまま FTS へ（これが OR バグの修正）
5. それ以外（＝ハイブリッド）          → FTS(長語を OR) + phase 2 で短語を LIKE 絞り込み
```

演算子検出は**前後にスペースを含む** `" AND "` / `" OR "` / `" NOT "` のまま
（`src/search.rs:637-640`）。裸の `contains("AND")` にすると `STANDARD` を誤判定する。

### 早期打ち切りの不変条件は保たれる

`search_conversations` の早期打ち切り（`src/search.rs:711`）は「フィルタは候補を
減らすだけで順位を上げない」ことに依存している。短語 LIKE は phase 2 の
`append_filters` と同じ位置に入る**減算的な述語**なので、この不変条件は変わらない。
コメントにその旨を追記する。

### 性能の最悪ケース（実測済み）

短語が実質どこにもマッチしない場合、早期打ち切りが発火せず全バッチを走査する
（grouped 経路は元々常に全走査 `src/search.rs:869-871`）。レビューで「それでも
速いというのは未検証の仮定だ」と指摘されたので実 DB で計測した。

| 候補数の多い長語 × ほぼ当たらない短語 | ハイブリッド | 現状（全表 LIKE） |
|---|---|---|
| `ください`（13,296 候補）× 当たらない短語 | **2.19s** | 10.81s |
| `します`（48,883 候補）× 当たらない短語 | **2.23s** | 7.57s |

最悪ケースでも現状より速い。候補が rowid で絞られている分、LIKE の評価対象行数が
必ず現状以下になるため。

## 実装

対象は `src/search.rs` のみ。`src/cli.rs` は変更不要（`--sort` は既に配線済み）。

### Step 1: FTS クエリ生成と語の分類を関数に切り出す

現状 `src/search.rs:637-654` と `:840-857` に同一ロジックが重複している。両方を
書き換えるので、ここで 1 箇所にまとめる。

```rust
/// クエリを FTS 経路の材料に分解する。
enum QueryPlan<'a> {
    /// 生のクエリをそのまま FTS へ（引用符・明示演算子つき）。
    Raw(String),
    /// 3 文字以上の語を OR 結合した FTS クエリと、LIKE で絞り込む短語。
    Hybrid {
        fts_query: String,
        short_terms: Vec<&'a str>,
    },
    /// FTS に渡せる語が無い。全表 LIKE + AND + 新着順。
    LikeOnly { terms: Vec<&'a str> },
}

/// 呼び出し側は `query` ではなく `trimmed` を渡す（現状 `:602` は `trimmed`、
/// `:637-642` は生 `query` を見ており、演算子検出と語分割の対象がずれている）。
fn plan_query(trimmed: &str) -> QueryPlan<'_>
```

`LikeOnly` が `terms` を持つのは、既存の LIKE ブロック（`:614-634` / `:810-813`）が
呼び出し側の `terms`（`:601` / `:788`）を参照しているため。分割を `plan_query` に
移すと呼び出し側から消えてコンパイルが通らない。

`Hybrid` の `short_terms` が空のとき、従来の純 FTS 経路と完全に同一になる
（この経路を別バリアントにしないのは、分岐を増やしても振る舞いが同じため）。

借用について: `short_terms` / `terms` は呼び出し側の `&str` から借りるだけで `self`
からは借りないため、後続の `&mut self` 呼び出し（`query_fts_rowids` `:661`、
`execute_search_typed` `:694`）と衝突しない。

### Step 2: `search_conversations` をハイブリッド対応にする

`src/search.rs:596-654` の分岐を `plan_query` の 3 バリアントに置き換える。
`QueryPlan::LikeOnly` は既存の `has_short_term` ブロック（`:614-634`）をそのまま使う。

phase 2 の SQL 組み立て（`:681-690`）に短語の LIKE を追加する。既存の
`escape_like`（`src/search.rs:238` 付近）を再利用する。

```rust
for term in &short_terms {
    sql.push_str(" AND m.full_content LIKE ? ESCAPE '\\'");
    batch_params.push(Box::new(format!("%{}%", escape_like(term))));
}
Self::append_filters(&mut sql, &mut batch_params, filter)?;
```

`extract_snippet` に渡す `search_terms`（`:739`）は全語のままでよい
（短語もハイライト対象であるべき）。

**却下した代案: Rust 側で `retain` する**。phase 2 は既に `m.full_content` を
取得している（`:685`）ので、SQL を触らずバッチ結果を `retain` でも同じ集合になる。
採らないのは意味論が割れるため — SQLite の `LIKE` は ASCII 大文字小文字を無視するが
Rust の `str::contains` はしない。同じ短語が `LikeOnly` 経路とハイブリッド経路で
違う挙動になるのは避ける。

### Step 3: `search_grouped_by_session` に同じ変更を入れる

`src/search.rs:787-857` の分岐を `plan_query` に置き換え、phase 2
（`:877-888`）に同じ短語 LIKE を追加する。早期打ち切りは引き続き入れない
（既存コメント `:869-871` を維持）。

### Step 4: ドキュメントとコードコメント

先の PR で書いた説明のいくつかが事実でなくなる。**消さずに書き直す**（全語短の
経路では AND + 新着順が残るため、非対称の理由づけ自体は生き続ける）。

- `src/search.rs:616-620` — LIKE 経路の非対称性を正当化するコメント
- `src/search.rs:760-762` — `search_grouped_by_session` の doc コメント
  （「any term under 3 characters」が誤りになる）
- テストの doc コメント `src/search.rs:1858-1860`, `:2866-2867`, `:3246-3248`
- `skills/conversation-search/REFERENCE.md:85-93` の警告。新しい規則は
  「3 文字以上の語が 1 つでもあればランキングが効く。短語は絞り込みにのみ使われる。
  全語が 3 文字未満のときだけ従来の AND + 新着順」。`パッケージ 更新 ドキュメント`
  の例も更新する。あわせて**大文字小文字の扱いが語の長さで割れる**ことを 1 行足す
  （長語は trigram の Unicode case folding、短語は `LIKE` の ASCII のみ）
- `CHANGELOG.md` の `[Unreleased]`。挙動変更 2 件（ハイブリッド化と `OR` 演算子
  バグ修正）。既存の行 18（短語は FTS を迂回する）は事実でなくなるので書き換える

## テスト

`setup_test_db()`（`src/search.rs:2127`）、`insert_test_message`、`from_connection`
で賄える。新しいハーネスは不要。

### 既存テストへの影響: なし（確認済み）

テストモジュール内の `search_conversations(` / `search_grouped_by_session(` 呼び出しを
全て確認した結果、**短語と長語が混ざったクエリを使うテストは 1 件も無く、裸の
`AND`/`OR`/`NOT` 演算子を使うテストも無い**。既存の LIKE 経路テストは全て全語短で、
新しい分岐でも `LikeOnly` のまま:

- `test_like_fallback_still_and_and_recency_ordered`（`:1862`、`"認証 実装"`）
- `test_search_short_query_like_fallback`（`:4013`、`"ab"` / `"型の"`）
- `test_search_cjk_2char_needs_like_fallback`（`:4062`、`"認証"`）
- `test_search_grouped_short_query_like_path`（`:2869`、`"ok"`）
- `test_quoted_phrase_bypasses_or_join_and_short_term_fallback`（`:3247`）は引用符
  経路（分岐 2）で不変

つまり期待値の書き換えは発生せず、doc コメントの文言修正だけ（Step 4）。
下記の追加テストは全て新規のガードである。

### 追加するテスト

- **T-H1 `test_hybrid_short_term_narrows_fts_results`** — 長語のみ含む行と
  長語＋短語を含む行を投入。短語込みクエリで後者だけが返ること。短語が
  「必須条件」であることを固定する
- **T-H2 `test_hybrid_ranks_by_bm25_not_recency`** — 古くて密な一致 vs
  新しくて長大な埋め草。短語を含むクエリで古い方が 1 位。この変更の存在理由
- **T-H3 `test_hybrid_all_short_terms_falls_back_to_like`** — 全語 2 文字で
  AND + 新着順が維持されること（意図的に残す挙動の固定）
- **T-H4 `test_or_operator_reaches_fts`** — `foo OR bar` が LIKE-AND ではなく
  FTS の OR として解釈されること。今回直す既存バグの回帰ガード
- **T-H4b `test_all_short_operator_query_stays_on_like_path`** — `更新 OR 認証`
  （全語短）が 0 件にならず従来どおり LIKE 経路に留まること。分岐 3 を分岐 4 より
  先に置いた理由そのものを固定する
- **T-H5 `test_hybrid_early_stop_respects_short_term_filter`** — 600 行程度を
  投入してバッチ境界を跨がせ、第 1 バッチが全て短語 LIKE で落ち、生存行が
  後続バッチにある構成にする。**埋め草は長語を高密度に含ませる**こと — そうしないと
  生存行が bm25 で batch 0 に来てしまい、早期打ち切りを一度も踏まずに緑になる。
  assert は「非空」ではなく「返る先頭行が生存行の中で最良 bm25 のものであること」
- **T-H6 `test_grouped_hybrid_match_count`** — grouped 経路で短語絞り込み後の
  `match_count` が正しいこと（grouped 側の実装漏れを検出する唯一のテスト）
- **T-H7 `test_hybrid_snippet_highlights_short_term`** — スニペットに短語の
  ハイライトが含まれること

## 検証

```bash
cargo test --all-targets --all-features
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

実 DB（`~/.conversation-search/index.db`）でのエンドツーエンド確認:

```bash
cargo build --release
B=./target/release/ai-conversation-search

# 1. 計測した改善が再現すること: 上位が実会話（短い）になる
time $B search "デプロイ 失敗" --limit 6

# 2. 短語が必須条件として効いていること（件数が長語単独より減る）
$B search "デプロイ" --limit 1000 --json | grep -c message_uuid
$B search "デプロイ 失敗" --limit 1000 --json | grep -c message_uuid

# 3. OR 演算子バグの修正: 136 件ではなく FTS の OR 相当になること
$B search "Docker OR リファクタ" --limit 1000 --json | grep -c message_uuid

# 4. 全語 2 文字は従来どおり（新着順・AND）
$B search "認証 実装" --limit 10

# 5. grouped でも効くこと
$B search "デプロイ 失敗" --group-by-session --limit 10

# 6. --sort=recent で従来の並びに戻せること
$B search "デプロイ 失敗" --sort=recent --limit 6

# 7. 全語短の演算子クエリが 0 件にならないこと（分岐 3 が 4 より先）
$B search "更新 OR 認証" --limit 5

# 8. 最悪ケースの性能（早期打ち切りが発火しない構成）
time $B search "します 畢" --limit 20
```

期待値: (1) 上位 6 件が実会話・0.2s 以内、(2) 減る、(3) 136 より大幅に多い、
(4) 現行 main と同じ、(5) 代表が最良スコア行、(6) 新着順。ただし**集合も変わる**
（今日は全語 LIKE-AND、変更後は FTS(長語 OR) ∩ 短語 LIKE）。(7) 非空、
(8) 3s 以内（現行 main は 7.5s）。

## 残る限界（今回スコープ外）

- **長語同士の網羅度がランキングに効かない**。`Docker Kubernetes 失敗` で
  `Docker`+`失敗` しか含まない行が、3 語すべて含む行より上位に来うる。bm25 に
  「何語カバーしたか」のボーナスが無いため。これは前 PR で OR を選んだ時点からの
  性質で今回の新規退行ではない。気になるなら後日ソートキーに一致語数を足す
- **短語が ASCII のときは絞り込みが弱い**。`LIKE '%ui%'` は `build` や `require` に
  も当たる。CJK では強く絞り、ASCII では緩い、という 1 つの規則の二面性がある
- `BATCH_SIZE = 500` + フィルタ + 短語 K 個のバインド変数。SQLite 3.32 以降の上限
  32,766 に対して安全（K が数百になるクエリは分岐 3 で `LikeOnly` に落ちる）

- **全語が 3 文字未満のクエリ**（`認証 実装` = 957 件）は救えない。trigram の
  3 文字制約に由来する。解消には tokenizer 変更（ICU / N-gram）が必要で、
  スキーマ移行と再インデックスを伴うため別途判断する
- `--exclude-project`（observer 除外）
- `stats.matched_messages` の意味論不整合（`search_conversations` は truncate 後、
  grouped は truncate 前）
