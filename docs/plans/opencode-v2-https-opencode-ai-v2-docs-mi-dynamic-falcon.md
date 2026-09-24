# OpenCode v2 対応（indexer のスキーマ追従）

> 実装後の変更: レビュー後の判断で v1 互換経路は削除し、v2 のみを読む形にした（下記「方針」の v1 維持は採用していない）。

## Context

OpenCode v2（手元 `opencode v2.0.11`）で `~/.local/share/opencode/opencode.db` のスキーマが変わり、
`src/indexer/opencode.rs` が参照する `session` / `message` / `part` テーブルが消えた。
代わりに `session_v2` / `session_message` がある（`sqlite3 .tables` で確認）。
現状は `oc_conn.prepare("... FROM session s ...")` が失敗し、`src/cli.rs:390` の
`Warning: failed to index OpenCode conversations` が出るだけで、v2 のセッションが一切インデックスされない。

移行ガイドの #plugins 節（V1 プラグインは V2 で動かない）は、このリポジトリには当てはまらない。
リポジトリに OpenCode プラグインは無く、OpenCode とは DB の読み取りだけでつながっているため。
DB パスは v2 でも同じなので、`OPENCODE_HOME` やドキュメントのパス記述は変更しない。

## v2 スキーマ（実 DB で確認済み）

- `session_v2`: `id, project_id, title, directory, time_created, time_updated, parent_id, ...`（`project` への FK はそのまま）
- `session_message`: `id, session_id, type, seq, time_created, time_updated, data`
  - `type`: `user` / `assistant` / `system` / `synthetic` / `shell` / `compaction` / `idle` / `agent-switched` / `model-switched`
  - `user.data`: `{"text": "...", "files": [], "agents": [], ...}`
  - `assistant.data.content[]`: `{"type":"text","text"}` / `{"type":"reasoning",...}` / `{"type":"tool","name":"shell","state":{"input":{"command":"..."}}}`
  - v1 との違い: part テーブルが無くなり、part は message の `content` 配列に埋め込まれた。tool 名のキーが `tool` から `name` に変わった
- v1 テーブルは v2 移行後の DB に残らない（`sqlite_master` に `session`/`message`/`part` が無い）

## 方針

`session_v2` の有無でスキーマを判定し、v1 と v2 の両方を読む。
v1 のまま使っているユーザーもいるので、v1 の経路は削除しない。
v1 経路は既存コードを流用するため、分岐のコストは小さい。

v2 で取り込む message は `type IN ('user','assistant')` だけ。
これは v1 で `role` が user/assistant 以外の message を捨てていた挙動と同じ。
`system` / `synthetic`（system-reminder）/ `shell`（!コマンドの出力）/ `compaction` はノイズなので取り込まない。
reasoning part も、v1 と同じく取り込まない。

## 変更内容（`src/indexer/opencode.rs` のみ）

1. `build_message_content(parts: &[(String,)])` を `&[serde_json::Value]` 受けに変更する。
   tool 名は `data.get("tool").or_else(|| data.get("name"))` で取り、v1/v2 の両方に対応させる。
   v1 経路では、part の JSON 文字列をパースしてから渡す。
2. スキーマ判定のヘルパーを追加する。
   ```rust
   fn is_v2(conn: &Connection) -> bool {
       conn.query_row(
           "SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_v2'",
           [], |_| Ok(())).is_ok()
   }
   ```
3. `do_index` の冒頭で `is_v2` を1回だけ呼び、v1 と v2 でセッションクエリを分ける。
   v2 では `session_v2.time_updated` をそのまま使えない。
   実 DB で確かめたところ、23 セッション中 22 件でメッセージの `time_updated` の最大値より古かった。
   これを同期カーソルにすると、追記されたセッションを取りこぼす。
   そのため、実効の更新時刻を次のように計算する。
   ```sql
   SELECT id, project_id, title, directory, time_created, time_updated, worktree FROM (
     SELECT s.id, s.project_id, s.title, s.directory, s.time_created,
            MAX(s.time_updated, COALESCE(
              (SELECT MAX(m.time_updated) FROM session_message m WHERE m.session_id = s.id), 0)
            ) AS time_updated,
            p.worktree
     FROM session_v2 s JOIN project p ON s.project_id = p.id)
   WHERE time_updated > ? ORDER BY time_updated DESC
   ```
   実効時刻は `last_sync_time` と `conversations.last_message_at`（再インデックスの要否判定、`opencode.rs:269-279`）の両方に使う。
   サブクエリは `session_message_session_time_created_id_idx` 経由になる（`EXPLAIN QUERY PLAN` で確認済み）。
   v1 のクエリは変更しない。
   テストできるように、セッションの取得は `fn fetch_sessions(conn, cutoff_ms, v2) -> Result<Vec<...>>` に切り出す。
4. `index_session` から message の取得部分を切り出す。
   `fn fetch_messages(conn, session_id_raw, v2) -> Result<Vec<(String /*id*/, String /*role*/, i64 /*time_created*/, String /*content*/)>>`
   - v1: 既存の message と part の 2 クエリ。role は `data.role` から取る
   - v2: `SELECT id, type, time_created, data FROM session_message WHERE session_id = ? AND type IN ('user','assistant') ORDER BY seq`
     - user: content は `data.text`
     - assistant: `data.content` 配列を `build_message_content` に渡す
   - v2 にはファイル変更専用の part 型が無い（実データは text / reasoning / tool の 3 種だけ）。
     edit / write は `[Tool: edit]` として記録される。
     `command` を持つのは shell だけなので、他のツールは名前だけ記録する。これは v1 と同じ制限
   - 空 content の除外と INSERT は既存ループに任せ、`index_session` 本体は共通のままにする。
     root/leaf message uuid は `messages[0]` / `messages.last()` をそのまま使う
5. テストを追加する（同ファイル `mod tests`）。
   - `build_message_content` に v2 形式の tool part（`"name":"shell"`）を渡し、`[Tool: shell]\nls` になること
   - in-memory SQLite に v2 のミニスキーマ（`project` / `session_v2` / `session_message` の必要な列だけ）を作り、
     `fetch_messages(conn, id, true)` を呼ぶ。system/synthetic が除外され、user/assistant の本文が `seq` 順で返ること
   - v2 のセッションクエリのテスト。セッションの `time_updated` がメッセージより古いとき、実効時刻がメッセージ側の値になること（取りこぼしの回帰テスト）
   - `is_v2` が v1/v2 それぞれのミニスキーマで true/false を返すこと

## 付随する変更

- `CHANGELOG.md` に `[Unreleased]` として追記する（Fixed: OpenCode v2 の DB を読めるようにした）
- バージョンは上げない（リリース時に `scripts/bump-version.sh`）

## スコープ外（メモに残す）

- OpenCode セッションの resume コマンド（v2 は `opencode -s <id>`）を `resume_command` に出す件。現状は `null` を返す（`src/cli.rs:1129`）
- 子セッション（`parent_id IS NOT NULL`、subagent 由来）の除外。v1 でも区別していないため
- v1 → v2 移行で OpenCode 側から消えた過去セッションは、既存の検索 index には残る（再インデックスで削除する処理は無い）

## 検証

1. `cargo test`（追加したテストを含む）と `cargo clippy`
2. ミューテーションで確かめる: v2 の `type IN (...)` フィルタや `name` フォールバックを外し、テストが落ちることを確認する
3. 実 DB でのスモークテスト:
   ```bash
   cargo build --release
   ./target/release/ai-conversation-search index --days 30
   ./target/release/ai-conversation-search list --source opencode   # v2 の 21 セッションが出る
   ./target/release/ai-conversation-search search "判定器" --source opencode
   ```
   `Warning: failed to index OpenCode` が出ないこと、タイトルが `判定器ログの出力先と保存場所` 等で出ることを確認する
