# v0.15.0 tech debt 一括解消プラン

## Context

v0.15.0（コミット `6384a69`、未 push）のレビューで「対応価値はあるがスコープ外」と判断した項目を、
リリース前に片付ける。判断軸は当時「フィードバックで指摘された調査動線の破損に直接効かないものは分離する」だったが、
タグをまだ切っていない今なら出力契約の破壊的変更もきれいに一度で通せる。この機会を逃すと互換レイヤーを抱えることになる。

対象は `memory/project_tech_debt_v0_15_0_review.md` の 13 項目。内訳は
**コード修正 10 件（T1–T9, T12）／テスト追加のみ 2 件（T10, T11）／対応不要 1 件（`init_schema`）**。
これに、レビューで新たに判明したリリース手順の穴を塞ぐ **T0** を加える。
すべて `yoshi47/v0.15.0-feedback-fixes` に積み増し、v0.15.0 として 1 本でリリースする。

### 調査で判明した、記録の誤り 2 件

- **`init_schema` の「全エラー破棄」は誤り** → 対応不要。`src/schema.rs:113` の `let _ = execute_batch(SCHEMA_SQL)` は
  意図的なベストエフォート第一パス（`data/schema.sql:46,102-103` が後発マイグレーションの列を参照するため未移行 DB では必ず失敗する）。
  マイグレーション後の **`src/schema.rs:140` が同じ SQL を `?` 付きで再実行**するので、ディスク full / read-only FS /
  schema.sql 破損はすべてそこで顕在化する。エラーが失われる経路は存在しない。
- **`list` が `messages.summary` を読む、も誤り**。`list_recent_conversations` は `SELECT * FROM conversations`
  （`src/search.rs:1622`）のみで、表示値は `conversations.conversation_summary`。
  したがって **`list` にランタイムのフォールバック（JOIN や追撃クエリ）は入れない**。空になる原因側だけを T12 で潰す。

---

## 作業項目

### 依存順

```
T0 wrapper 版数チェック ─┐
T1 quote ────────────────┤
T2 env parse             │
T3 migration detect      ├─→ T7 JSON envelope ─→ T9 --content ─→ T12 docs
T4 prune 確認 → T5 scan  │
T6 list truncation ──────┘
T8 tree exit（独立）
T10 / T11 テスト（独立）
```

- **T1 は T7 より前**: `resume_command` の文字列が変わる。envelope のテストを二度書かないため。
- **T6 は T7 より前**: `ListResult` が `truncated` を持てば、T7 は `cmd_list` の JSON 分岐だけを触れば済む。逆順だと `search.rs` を再度開くことになる。
- **T9 は T7 より後**: `full_content` は envelope の `results[]` の中に入る。逆順だと裸配列に足してから移すことになる。
- **T0 は T7 と同一リリース**: T0 が無いと T7 の不整合が無言で「0 件ヒット」に化ける（下記 T0 参照）。
- **T5 は T4 より後**: 同じ関数を触る。
- T2 / T3 / T8 / T10 / T11 は他から独立。

---

### T0. ラッパーとバイナリの版数不整合を検出可能にする

**これはレビューで新たに見つかった項目で、T7 の前提を守るためのもの。**

`bin/ai-conversation-search:11` はキャッシュパスをバージョン文字列だけで決め、`:90` は
`[ ! -x "$BINARY" ]` の**存在チェックのみ**（チェックサムも版数照合もない）。
そのため「そのバージョンのファイルが既にある」だけで再ダウンロードが起きない。

**現に今この環境で発生している**: `~/.conversation-search/bin/ai-conversation-search-0.15.0` が
本日 12:48 のローカルビルド（envelope 導入前）として存在する。同じディレクトリの
`ai-conversation-search-0.13.0.stale-local-build.bak` は、同じ事故が過去に一度起きた証拠。

T7 適用後にこの状態で動かすと: 旧バイナリが裸配列を出す → 新ラッパー `:225` の `.results[]` が jq エラー →
`:234` の `|| true` が握り潰す → `INITIAL` が空 → `:272-281` が
**「No sessions found in the last N days. Try loosening --days, --repo, or --source」** と表示して exit 1。
無言で、しかも積極的に誤った診断を出す。

対応 2 点:

1. `:90` の存在チェックに版数照合を足す。`"$BINARY" --version` の出力が `$VERSION` を含まなければ再ダウンロードする。
   これで `ACS_VERSION`（`:9`）による意図的な差し替えも、将来のリテイクも、無言破綻ではなく再取得になる。
2. **リリースタグは一度きり**にする。ユーザーが一度実行した後に v0.15.0 を打ち直すと、
   旧バイナリ + 新ラッパーの組み合わせが恒久的に残る（1 の版数照合はローカルビルドのように
   バージョン文字列が同じケースを検出できない）。

なお **v0.15.0 はまだ一度も publish されていない**ので、外部ユーザーに汚染キャッシュは存在しない。
壊れているのはこの開発機のみで、検証手順の冒頭で消す（下記「検証」参照）。

---

### T1. `resume_command` のシェルクォート（セキュリティ）

`src/cli.rs:759` が `format!("cd {} && {} --resume {}", pp, cmd, sid)` で `project_path` を素で埋め込む。
README.md:222 / SKILL.md:293 が `eval "$(ai-conversation-search pick)"` を案内しているため、
空白入りパスで `cd` が別ディレクトリに落ち、`;` や `$(...)` を含むディレクトリ名なら任意コマンドが走る。

**方針: 条件付きクォート（shlex 相当）+ `--` によるオプション終端。**
安全な文字だけなら素通し、それ以外は `'...'` で包み内部の `'` を `'\''` に置換。
無条件クォートを採らないのは、正常系の全パスが `'…'` になって既存の README/REFERENCE の例とテストが一斉に陳腐化するため。

```rust
/// Quote a string for safe interpolation into a POSIX shell command.
///
/// `resume_command` is documented as something you `eval` (README.md:222, SKILL.md:293), and
/// `project_path` comes from `sessions-index.json` file content (src/indexer/claude_code.rs:1139-1145),
/// not from a validated source. Conditional rather than unconditional so normal paths and
/// UUIDs stay byte-identical to what the docs show.
fn shell_quote(s: &str) -> String {
    fn is_safe(c: char) -> bool {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '/' | '-')
    }
    if !s.is_empty() && s.chars().all(is_safe) {
        return s.to_string();
    }
    // Single quotes suppress every expansion; the only character they cannot contain is
    // `'` itself, spliced back in as `'\''` (close, escaped quote, reopen).
    format!("'{}'", s.replace('\'', r"'\''"))
}
```

**クォートだけでは塞がらない穴が 2 つある**（レビュー指摘、いずれも対応する）:

- **先頭 `-`**: `-Users-foo` は allowlist を全通過して素通しになるが、`cd '-Users-foo'` もクォート除去後は
  `-Users-foo` で `cd` のオプションとして解釈される。**`cd -- {}` / `--resume -- {}` ではなく
  `cd -- {}` と `--resume {}` の前に `--` を置く形にする**（`claude --resume -- <id>` が有効かは実装時に要確認、
  無効なら session_id 側は「先頭 `-` を含む値は allowlist から外して必ずクォート」+ 別途拒否）。
  `session_id` はファイル名ではなく**トランスクリプト本文の `sessionId`**（`src/indexer/claude_code.rs:1106`）由来なので、
  `-` 始まりの値は表現可能。
- **制御文字**: 改行・タブ・NUL は `is_ascii_alphanumeric()` を通らないのでクォート側に落ちるが、
  NUL は `'…'` の内側に出力され、消費側シェルがそこで truncate して**クォートが閉じない**。
  `is_safe` から明示的に制御文字を除くだけでなく、**制御文字を含む `resume_command` は `null` を返す**方が安全。
  併せて `bin/ai-conversation-search:226` の `\(.resume_command // "")` は表示列（`:233`）と違い `gsub` されていないため、
  改行を含むと行が割れて `cut -f2`（`:297`）が断片を返し、それが `eval` される。**ラッパー側でも `gsub` する。**

`claude_cmd()`（`src/cli.rs:27-29`）は**クォートしない** — `$CC_CONVERSATION_SEARCH_CMD` はフラグを含みうるシェル片で、
包むと動いていた設定が `command not found` になる。ただし Claude Code はプロジェクトの `.claude/settings.json` で
`env` を設定できるため、`PATH` と違い**リポジトリ側から影響を受けうる**。この信頼境界をコメントに明記する。

適用先は **4 箇所**（3 箇所ではない）:

| 箇所 | 内容 |
|---|---|
| `src/cli.rs:759` | JSON の `resume_command` |
| `src/cli.rs:883-884` | `cmd_search` 人間向け `Resume:` ブロック |
| `src/cli.rs:962-963` | **`cmd_search_grouped` 人間向け `Resume:` ブロック**（picker が使う経路） |
| `src/cli.rs:1119-1120` | `cmd_resume` の 2 行 |

`bin/ai-conversation-search:191` の `tree {1} --json` は **変更不要**。fzf は `{n}` 展開時に自動でシングルクォートする
（生展開は `{r1}` を明示した場合のみ）、かつ field 1 は自前 JSON の `session_id`。`:190` のコメント末尾に確認済みの旨を 1 行足す。

テスト（`src/cli.rs` の `mod tests`）: 通常パス素通し / 空白 / `$(...)` / バッククォート / `;` `|` / 埋め込み `'` /
空文字（`''` になること）/ 先頭 `-` / 改行・タブ / 日本語パス（クォート側に落ちること）/
`inject_resume_command` 経由の統合 1 本。

---

### T2. `CONVERSATION_SEARCH_INDEX_OBSERVER` の真偽値解釈

`src/indexer/claude_code.rs:28-32` が `.map(|v| v == "1")` で、`=true` を無言で無視する。

`parse_env_flag(raw) -> Option<bool>` を追加し、`1/true/yes/on` を true、`""/0/false/no/off` を false、
それ以外は **`None` → stderr に警告して false**。無言 false がまさにバグなので警告は必須。
`""` を false 側に入れるのは `VAR= cmd` がインライン無効化の慣用だから。

テストは `parse_env_flag` の純粋関数テストのみ。`std::env::set_var` は使わない
（既存の `src/indexer/opencode.rs:476-489` が無防備で並列ハーネス下で競合するため、二例目を作らない）。

---

### T3. `detect_custom_migration_applied` のエラー握り潰し

`src/schema.rs:234,248` の `.optional().unwrap_or(None)` → `:235,249` の `is_none_or(...)` により、
**クエリ失敗が「オブジェクト不在」と区別できず `true`（＝適用済み）** になる。`true` は
`bootstrap_existing_db:194` で `record_migration` を呼び、`:119` の `is_migration_applied` が以後永久にスキップさせる。

SQL 側（`detect_sql_migration_applied:203-221`）は失敗時 `false` に倒れてマイグレーションが走り、
冗長な `ALTER TABLE` が大声で失敗する。**安全でない方向に倒れるのは Custom 側だけ**なので、そこだけ直す。

戻り値を `Result<bool>` にし、v8/v9 の分岐を `(obj_type, obj_name, marker)` のタプルに畳んで
`sqlite_master` の 1 クエリに統合。`.optional()?` + `.flatten()` にする
（`sqlite_master.sql` が NULL の合法な形で `InvalidColumnType` を握り潰していた分も顕在化させる）。
呼び出し側 `src/schema.rs:191-196` に `?` を付ける。

**トレードオフを明記**: 今まで「無言で間違った状態」だったものが、`search`/`list`/`tree`/`status` の
**ハードエラー（exit 1）** になる。`bootstrap_existing_db` の早期 return（`:176-179`）が効くのは
`schema_version` が空のときだけなので「DB あたり 1 回」なのは *bootstrap* であって *失敗* ではない。
そのためエラーメッセージには **DB パスと復旧手段（`init --force` 等）を含める**。
Stop hook は影響を受けない（`cmd_hook` → `maybe_background_index()` が `let _ =` で握り潰す、`src/cli.rs:285-287`）。

テスト: fresh DB で v8/v9 とも `true` / レガシー `messages_ad` を再インストールして v9 が `false`
（`test_migration_9_replaces_legacy_delete_trigger`（`src/schema.rs:534-567`）の手口を踏襲）。
**エラー注入テストは作らない** — `sqlite_master` の読み取りを失敗させるには rusqlite の authorizer が要り、
`hooks` feature を有効化していない（`Cargo.toml` は `rusqlite = { version = "0.38", features = ["bundled"] }`）。
テスト 1 本のために feature を足すのは割に合わない。

---

### T4. `prune-observer` の確認プロンプト

`src/cli.rs:118-123` は `--dry-run` のみ、`:576-628` は初回実行で即削除。
`--yes` を追加し、非 TTY かつ `--yes` なしは **拒否して exit 1**（`main.rs:17-20` が `Err` を exit 1 にする）。
「無回答」を「yes」と読ませない。TTY 判定は `std::io::IsTerminal`（`Cargo.toml` は edition 2021・MSRV 固定なし）で足り、**依存追加は不要**。

呼び出しはバックアップ警告 4 行（`src/cli.rs:606-612`）の**後**、`prune_observer_sessions()`（`:613`）の前。
`count == 0` 分岐（`:592-601`）は `rebuild_fts` のみで非破壊なのでプロンプトなし。

**ドキュメント追従が必須**（レビュー指摘）: Claude Code の Bash ツールは常に非 TTY なので、
`skills/conversation-search/REFERENCE.md:299-304` と `README.md:202-205` の synopsis をそのまま実行するエージェントは
今後ハードエラーになる。両方に `--yes` を反映する。
**判断が要る点**: エージェントに不可逆操作の `--yes` を渡させてよいか。
デフォルトは「ドキュメントは `--dry-run` を先に案内し、`--yes` は人間の確認後に使う」と書く方針とする。

`prune-observer` を呼ぶ自動化はリポジトリ内に存在しない（`hooks/`・`bin/`・`tests/`・`.github/` を検索して
`src/cli.rs:347` と散文のみ）ので、CI・フックの破壊はない。

テスト: `confirm_prune(n, true)` が stdin に触れず `Ok(true)`、`confirm_prune(5, false)` が
`Err`（テストハーネスの stdin は非 TTY＝まさに検証したい性質）である旨をコメント付きで固定。

---

### T5. `prune-observer` の全表スキャン削減

記録は「2 回走る」だったが、実測すると **削れるのは dry-run のみ**:

- dry-run: `count_observer_sessions`（`src/cli.rs:578`）+ `sample_observer_sessions`（`:585`）＝ 2 回
- 破壊系: `count_observer_sessions`（`:578`）+ `prune_observer_sessions` 内の `CREATE TEMP TABLE`
  （`src/indexer/claude_code.rs:1423-1426`）＝ 2 回

破壊系の事前 count は無駄ではない（T4 のプロンプトと `count == 0` 短絡がそれに乗る）。**dry-run だけ 1 回にする。**

`sample_observer_sessions`（`src/indexer/claude_code.rs:1379` 付近）を
`survey_observer_sessions(sample_limit) -> Result<(i64, Vec<...>)>` に置き換え、
temp table を 1 回作って count と sample を両方そこから取る。`count_observer_sessions` は破壊系とテストが使うので残す。
`CREATE TEMP TABLE` の前に **`DROP TABLE IF EXISTS`** を付ける（temp table は接続スコープで
`:1423-1426` と名前を共有するため、途中エラーで残ると次回を汚す）。`:1423` 側にも対称に付ける。

`cmd_prune_observer` は `count_observer_sessions()` の呼び出しを dry-run 分岐の**下**へ移動する。

テスト: `seed_observer_and_normal_rows`（`src/indexer/claude_code.rs:2491` 付近）で count/sample を検証し、
**2 回連続で呼んで** temp table が後始末されていることを固定。

---

### T6. `list` の打ち切り検出（T7 の前提）

`src/search.rs:1654-1655` の `LIMIT ?` が無言で切り、戻り値は素の `Vec`（`:1595-1598`）、`cmd_list` は
`print_truncation_notice`（`src/cli.rs:705-714`）を呼んでいない。

`ListResult { rows, truncated }` を `SearchResult`（`src/search.rs:219-224`）の隣に追加し、
`list_recent_conversations` を `Result<ListResult>` に変更。over-fetch by one は
`src/search.rs:713-717`（LIKE パス）/ `:910-923`（grouped）の既存イディオムをそのまま踏襲する。

**`SearchStats` は流用しない** — `gather_search_stats` は `append_filters` 経由で `messages.timestamp` を条件に
メッセージ単位の総数を数えるが、`list` は `conversations.last_message_at` で絞る。別の問いに答える数字が 3 本の
COUNT クエリ付きで付いてくることになる。

`limit < 0` は `src/search.rs:685,867` と同じ文言で `Err` にする（`list --limit -1` が「無制限」なのは SQL の偶然で未文書化）。
`Err` を先に返すので `rows.truncate(limit as usize)` で足りる（`limit.max(0)` は到達不能なので入れない）。

**`cmd_list` の通知は 3 箇所必要**（レビュー指摘）。`cmd_list` は `src/cli.rs:1035`（JSON 分岐）と
`:1040`（空）で早期 return するため、ループ後に 1 回置くだけではどちらでも発火しない。
`cmd_search` が `:801` / `:816` / `:833` の 3 箇所を持つのと同じ理由。

既知の限界としてコメント 1 行: `query_rows` は行マップ失敗を `log::warn!` で捨てる（`src/search.rs:1136-1138`）ので、
+1 行目のマップが失敗すると truncation を過少報告する。既存パスも同じで、ここでは直さない。

テスト: `test_list_recent_conversations`（`src/search.rs:4606` 開始、アサーション 3 箇所 `:4632` `:4641` `:4652`）を
`.rows` に追従。新規に「3 件・limit 2 → rows 2 + truncated」「3 件・limit 3 → rows 3 + not truncated」「limit -1 → Err」。
戻り値型が public なので、他の `list` テストにも波及する前提で見積もる。

---

### T7. `--json` を envelope 化して `truncated` を載せる（破壊的変更）

現状 `search` / `search --group-by-session` / `list` は裸の配列で、打ち切り通知は stderr のみ。
SKILL.md:273 が「Always use `--json`」と指示しているため、stdout しか読まない消費側は打ち切りに気づけない。

**採用: 3 コマンド共通の envelope。**

```json
{ "results": [ … ], "truncated": false }
```

- キーを `results` に統一するのは、3 サブコマンドで同じ jq 式が使えるようにするため。
- **`count` は入れない**。`results | length` と冗長な上、`{"count": 20, "truncated": true}` は
  「全ヒット数が 20」と誤読される（レビュー指摘）。曖昧なキーを増やすより無い方がよい。
- `tree` / `context` / `status` は既にオブジェクトなので**変更しない**。

**採らなかった案**: (a) オプトイン `--json-envelope` は既定出力が永久に間違ったままになり、
契約が二股になって REFERENCE.md が両モードを説明する羽目になる。
(b) 行ごとに `_truncated` を付けるのは全体の事実を N 回繰り返す上、`--limit 0`（打ち切りが最も紛らわしい場面）で
空配列になり表現できない。

`print_json_envelope` ヘルパを `src/cli.rs` に置き、**`inject_resume_command` は包む前の内側配列に対して**走らせる。
`inject_resume_command`（`:731-768`）はオブジェクトの値へ降りない設計で、降りるように直すのは却下
（`tree`/`context` の JSON が持つ `conversation` オブジェクトにも `session_id` があり、
`resume_command` が副作用で生えてしまう）。この理由をコメントに残す。

差し替え: `src/cli.rs:798-802`（search）/ `:904-908`（grouped）/ `:1030-1035`（list）。
`print_truncation_notice` の stderr 出力は**残す**（jq に流している人間にはまだ有用で、消す理由がない）。

**消費側の追従（同一コミット必須）**:

| 対象 | 変更 |
|---|---|
| `bin/ai-conversation-search:225` | `.[] \|` → `.results[] \|` |
| `bin/ai-conversation-search:222` `:223` | フォールバック `out="[]"` → `out='{"results":[]}'`（2 箇所とも） |
| `bin/ai-conversation-search:226` | `\(.resume_command // "")` に `gsub` を追加（T1 の改行対策） |
| `bin/ai-conversation-search:254-261` | 変更不要（stdout を捨て exit status だけ見る）。`--limit 1` で truncation 通知が毎回 stderr に出て `$PROBE_ERR` に入る点をコメントで明記 |
| **`tests/test_pick.sh:129`** | **`jq 'length'` → `jq '.results \| length'`。envelope ではキー数 2 を返して `-gt 0` が通り、テストが無言で無意味になる**（唯一「静かに壊れる」箇所） |
| `tests/test_pick.sh:141,200-211,219,266` | jq 式に `.results` を挿入（`:220` は `:219` が配列に再ラップ済みなので変更不要） |
| `tests/test_pick.sh`（`:129` 付近に新規） | envelope 形状テスト（`type == "object"`、`.truncated \| type == "boolean"`） |
| `README.md:292` | 例を `jq '.results[] \| .conversation_summary'` に |
| `README.md` `--json` 節 | envelope の説明を追記 |
| **`skills/conversation-search/REFERENCE.md:401-417`, `:420-430`** | **裸配列の実例ブロックそのものを envelope に書き換える**（散文の追記だけでは、エージェントが読む実例が変更前の契約を示したままになる） |
| `skills/conversation-search/SKILL.md` | 形状を 1 文で明記（現状 `--json` の記述は約 25 箇所あるが形状の説明がゼロ。REFERENCE.md を読まないエージェントが試行錯誤で発見する羽目になる） |
| `CHANGELOG.md` | 破壊的変更である旨と、長時間動いているセッションは再起動が要る旨 |

---

### T8. `tree` のエラーを stderr + exit 1 に

`cmd_tree`（`src/cli.rs:1065-1091`）は JSON 分岐で exit 0、人間向け分岐も `println!("Error: …")` を **stdout** に出して exit 0。
`cmd_resume`（`:1122-1125`）は stderr + exit 1 で不整合。スクリプトが `$?` を見ると
「曖昧で選べなかった」を「空の会話」と読む。

**JSON 本文は 1 バイトも変えない**（既存の `error` キー読者と fzf preview を守る）。変えるのは exit code と人間向けの出力先だけ。
`tree_exit_code(&ConversationTree) -> i32` を純粋関数として切り出す（`cmd_tree` は実 DB を開き `process::exit` するのでテスト不能）。
`warning` は **exit 0 のまま** — 部分データは返っているので、非ゼロにすると読むべき出力を捨てさせることになる。
`process::exit` はデストラクタを飛ばすため、直前に stdout を明示 flush する。

**fzf preview は壊れない。ただし理由はパイプラインの終了ステータスではない**（レビュー指摘）:
fzf の `--preview` 文字列は fzf 自身が `$SHELL -c` で実行し、**preview の終了ステータスを一切見ない**。
ラッパーの `set -e`（`:6`）はそもそも適用されない。
したがって「将来 `set -o pipefail` を足すと壊れる」というコメントは**誤りなので書かない**。

**既存 3 テスト（`src/search.rs:4232-4241`, `:4402`, `:4422`）は変更しない。** これらは
`get_conversation_tree` が返す `ConversationTree::error` を検証しており、in-band であること自体が検索層の契約
（検索層が `process::exit` を呼ぶべきではない）。変わるのは CLI 側の解釈だけ。**これらに修正が要るなら T8 が検索層に漏れている合図。**

`tree` の呼び出し元一覧: `bin/ai-conversation-search:191`、`tests/test_pick.sh:270`、
docs は `SKILL.md:156,264,267` / `REFERENCE.md:241-253` / `README.md:195`。
`test_pick.sh:270` は `|| echo ""` で吸収されるので既存分は無事。

テスト: `tree_exit_code` の 3 ケース（error → 1 / warning のみ → 0 / clean → 0）。
`tests/test_pick.sh` に「存在しない id で exit 1 かつ `.error` がパースできる」の 1 本。
**`tests/test_pick.sh` は `set -e`（`:8`）なので、新テストは必ず `if ! … ; then` か `|| true` で囲む**
（裸で exit 1 するコマンドを書くとスイート全体が中断する）。
REFERENCE.md に「`error` は exit 1、`warning` は exit 0」を明記する。

---

### T9. `--content` の実態と名前の乖離

現状: `src/cli.rs:862-866` が 300 文字で切り、**長さに関係なく `"..."` を付ける**。
`--json` では JSON 分岐が `:809` で return するため無視され、`--group-by-session` では
`cmd_search_grouped`（`:892-898`）に引数がなく捨てられる。

**採用: (i) 動くようにする + 省略記号の修正 + `--content-chars` 追加。**
(ii)「`--content --json` はエラー」は、JSON でも意味は明確なのに拒否するのが恣意的で、既存スクリプトを壊す。
(iii) リネーム/再文書化は、`search --json` からメッセージ本文を取る手段が消え、N 回の `context` 追撃を強いる。

**サイズを見積もった上での方針変更**（レビュー指摘）: JSON でも `--content-chars` を**適用する**。
N+1 クエリ自体は問題ない（`get_full_message_content`（`src/search.rs:1662-1670`）は索引付きの点検索）。
問題は出力量で、REFERENCE.md:382 によればユーザーメッセージは平均 3.5K 文字。
`search --limit 50 --content --json` は約 175KB、`--limit 1000` なら数 MB が、
「Always use `--json`」と書かれた skill 経由でエージェントのコンテキストに流れ込む。
**上限なしは採らない。人間向けと JSON で同じ `--content-chars`（既定 300）を使い、必要なら明示的に上げる。**

- `truncate_chars(s, max) -> (String, bool)` を追加（`chars()` 必須 — コーパスが日本語中心でバイト境界 panic する）。
  切れたときだけ `…` を付ける。
- `--content-chars`（既定 300）を **`src/cli.rs:163`（`content: bool`）の後**に追加し
  （`:162` は `#[arg(long)]` 属性行なので、その直後に挿すと属性とフィールドが分離してコンパイルできない）、
  `cmd_search` / `cmd_search_grouped` に通す。`--content` は on/off のまま残す。
- `--json` では `full_content`（`--content-chars` で切り詰め済み）+ `full_content_truncated: bool` を各行へ付ける。
- `inject_full_content` は再帰させず、grouped の `representative.message_uuid` は `row_uuid()` 補助関数で拾う
  （再帰は `inject_resume_command` と同じ罠を踏む）。
- `context --content`（`src/cli.rs:184-186,1013-1014`）は元から無切り詰め。非対称が意図的である旨をヘルプ文言に書く。

**ドキュメント追従（必須）**: `skills/conversation-search/REFERENCE.md:70` と `SKILL.md:240` は
「`--content` は human output only、`--json` では無視される」と明記しており、T9 はこれを偽にする。両方書き換える。

**コミットは 2 本に割る**（レビュー指摘、bisect のため）:
(a) `truncate_chars` + 省略記号修正 + `--content-chars`、(b) `--json` / `--group-by-session` で効かせる。

テスト: `truncate_chars` の 5 ケース（max 未満で無印 / ちょうどで無印＝今回のオフバイワン / 超過で `…` /
マルチバイト境界 / max 0）。

---

### T10. observer ディレクトリスキップの e2e テスト（テストのみ）

`src/indexer/claude_code.rs:568-578` のスキップは、`scan_conversations`（`:511`）が `discover_project_dirs()` で
`$HOME` を読むためテストできない。`:510` を 2 段に割り、`scan_project_dirs(&[PathBuf], days_back)` を切り出す。

テストは **scan の戻り値**を検証する（sync_state ではなく）。indexer を一切走らせないので、
`:1069` のファイル単位バックストップがリグレッションを覆い隠せない。
observer ディレクトリ名は `OBSERVER_PROJECT_DIR_SUFFIX`（`:21`）から組み立てる（定数変更でテストが空振りしないため）。
`days_back: None` で `:528` の mtime カットオフを回避。`set_index_observer(true)` で逆方向（escape hatch が効く）も固定する。

---

### T11. 「最初の user メッセージ」定義の差分（テストのみ、コード変更なし）

Rust 側（`src/summarization.rs:165-172`）は配列順、SQL 側（`src/indexer/claude_code.rs:52-63`）は
`ORDER BY timestamp ASC, message_uuid ASC`。**揃えるのは不可能** — `messages` は `depth`（根からの距離）は持つが
挿入順を持たないので、SQL を配列順に合わせるには新カラムとバックフィルが要り、差分そのものより risk が大きい。

露出も既に限定的: `test_prune_observer_ignores_marker_after_first_message`（`:2575-2612`）が `rn = 1` を固定しており、
偽陽性には「timestamp 最古の user メッセージがマーカーを含む非 observer セッション」が必要で、それはほぼ observer の定義そのもの。

**両実装を残し、`:38-51` のコメントも残し、両者が実際に食い違いうる唯一のケース
（transcript 順 ≠ timestamp 順 = resume されたセッションや sidechain）の差分テストを 1 本追加する。**
書いて落ちたらそれが発見であり、そのとき方向を決める。
実 observer transcript のフィクスチャは**作らない**（誰かのセッション内容をリポジトリに入れることになる）。

---

### T12. `conversation_summary` 空白のハードニング

`conversations` への書き手は 3 箇所のみで、いずれも非 NULL のフォールバックを束縛している:
`src/indexer/claude_code.rs:1148-1159`（5 段フォールバック → `"Untitled conversation"`）、
`src/indexer/opencode.rs:381`、`src/indexer/codex.rs:444-447`。
`[no summary]` に到達するのは (a) フォールバック導入前のバージョンが書き、再インデックスされていない行
（`:1028` の sync_state スタンプが未変更ファイルの再読込を止める）、(b) 上流タイトルが `""` か空白文字列で
`unwrap_or` をすり抜けた場合、だけ。

`list_recent_conversations` に LEFT JOIN や行ごとの追撃クエリを入れるのは、ほぼゼロのケースのために全行にコストを払う。
JOIN 形はさらに悪い（SQLite が `ORDER BY … LIMIT` のソータより先に出力式を評価しうるので「`--limit` で有界」の直感が成り立たない）。

**症状ではなく原因側を 2 箇所だけ直す**:
1. `display_summary`（`src/cli.rs:770-775`）を `!s.trim().is_empty()` に。search / grouped / list を 1 箇所でカバー。
2. インデクサのフォールバックが空白を弾くよう `.filter(|s| !s.trim().is_empty())` を
   `src/indexer/claude_code.rs:1148-1159` の各段、`src/indexer/opencode.rs:381`、`src/indexer/codex.rs:444` に足す。

---

## コミット分割

1. `fix: ラッパーがキャッシュ済みバイナリの版数を照合するようにする`（T0）
2. `fix: resume_command をシェルクォートしオプション終端を付ける`（T1、4 箇所 + ラッパーの gsub）
3. `fix: observer 環境変数の真偽値解釈を寛容にする`（T2）
4. `fix: マイグレーション検出のクエリ失敗を握りつぶさない`（T3）
5. `fix: prune-observer に確認プロンプトと --yes を追加`（T4、README/REFERENCE 同梱）
6. `perf: prune-observer --dry-run の全表スキャンを1回にまとめる`（T5）
7. `feat: list の --limit 打ち切りを通知する`（T6）
8. `feat!: search/list の --json を results/truncated の envelope にする`（T7、ラッパー・test_pick.sh・docs 同梱）
9. `fix!: tree の解決失敗を stderr + exit 1 にする`（T8）
10. `fix: --content の省略記号と --content-chars を整える`（T9a）
11. `feat: --content を --json と --group-by-session でも効かせる`（T9b、docs 同梱）
12. `test: observer ディレクトリスキップと検出定義の差分を固定する`（T10, T11）
13. `fix: 空白のみの conversation_summary を [no summary] として扱う`（T12）
14. `docs: CHANGELOG を 0.15.0 の最終仕様に合わせる`

---

## 検証

**最初に必ず実行**（T0 の項で述べた汚染キャッシュの除去。これを忘れると以降の検証が全部嘘になる）:

```bash
mv ~/.conversation-search/bin/ai-conversation-search-0.15.0 \
   ~/.conversation-search/bin/ai-conversation-search-0.15.0.stale-local-build.bak
```

各コミットごと:

```bash
cargo fmt --check && cargo clippy -- -D warnings && cargo test
```

全体完了後:

```bash
cargo build --release

# tests/test_pick.sh は $HOME のキャッシュパスからバイナリを探し（:12）、
# 無ければ SKIP して exit 0 する（:36-39）。ビルド成果物を置いてから走らせないと
# 「envelope をテストしたつもりで何もテストしていない」状態になる。
cp target/release/ai-conversation-search ~/.conversation-search/bin/ai-conversation-search-0.15.0
./tests/test_pick.sh

# tree の失敗契約
./target/release/ai-conversation-search tree no-such-session --json; echo "exit=$?"   # exit=1、stdout に .error

# envelope 形状
./target/release/ai-conversation-search search "test" --json | jq '{truncated, n: (.results|length)}'
./target/release/ai-conversation-search list --days 7 --limit 2 --json | jq '.truncated'

# env var（status は ConversationIndexer を作らないので警告が出ない。
# maybe_background_index も detached child なので stderr が手元に来ない。前景の index で確認する）
CONVERSATION_SEARCH_INDEX_OBSERVER=true  ./target/release/ai-conversation-search index --days 1  # 警告なし
CONVERSATION_SEARCH_INDEX_OBSERVER=maybe ./target/release/ai-conversation-search index --days 1  # 警告あり

# prune-observer の非 TTY 拒否
# 注意: observer セッションが 0 件の DB では count==0 分岐（src/cli.rs:592-601）が
# プロンプト前に return するため exit 0 が正しい。拒否を見るには observer 行のある DB が要る。
./target/release/ai-conversation-search prune-observer --dry-run          # 件数を確認
./target/release/ai-conversation-search prune-observer < /dev/null; echo "exit=$?"  # 1件以上なら exit=1
```

シェルクォートの手動確認（自動テストで再現しづらいので必ず実施）:

```bash
mkdir -p "/tmp/acs test;echo pwned"   # 空白と ; の両方を含むパス
# そこに会話を 1 本作ってインデックスしたうえで
./target/release/ai-conversation-search search "…" --json | jq -r '.results[0].resume_command'
# → cd -- '/tmp/acs test;echo pwned' && claude --resume … の形になり、eval しても pwned が出力されないこと
```

リリース前（CLAUDE.md 必須手順）:

- `tests/skill-discovery/scenarios.md` のシナリオを **新規 Claude Code セッション**で 1 つ実行し、
  `conversation-search` skill が選ばれることを確認する
- `git tag v0.15.0 && git push origin v0.15.0` — **打ち直さない**（T0 参照）

## 見送り（tech debt に残す）

- `init_schema` 第一パス（実害なし。`log::debug!` を足すかは実装時の任意）
- `list_recent_conversations` の runtime summary フォールバック（入れるなら JOIN ではなく空欄行だけの後追いクエリ）
- `search.rs` の SELECT 句重複（`:701-703` / `:949-954` / grouped の内側 CTE）
- `bin/ai-conversation-search` のキャッシュにチェックサム検証がない点（T0 は版数照合まで。
  同一バージョン文字列のローカルビルドは検出できない）
