# v0.16.0: UUID 直指定ケースの摩擦をなくす

> **For agentic workers:** 上から順に実行する。各タスクは「失敗するテストを書く → 走らせて落ちることを確認 → 実装 → 走らせて通ることを確認 → コミット」の順。テストは必ず**ミューテーション**（実装を戻して落ちるか）で確かめること。行番号は 2026-08-19 時点の `main`（858b323）基準。

**Goal:** セッション UUID を渡された AI エージェントが、追加のインデックス操作も jq の手書きもなしに、1 コマンドで目的のメッセージだけを取り出せるようにする。

**Tech stack:** Rust 2021 / clap 4 / rusqlite 0.38（bundled SQLite, FTS5 trigram）/ serde_json。テストは全て inline `#[cfg(test)] mod tests`（`tests/*.rs` は無い）。

**Architecture:** `src/cli.rs` が clap 定義と全コマンドハンドラ、`src/search.rs` が読み取り、`src/indexer/claude_code.rs` が書き込み。本計画は search.rs の公開 API を変えず、CLI 層の後処理とインデクサの単一ファイル索引で完結させる。

---

## Context

利用者から「検索は速くて正確だが、セッション UUID を直に渡すケースで摩擦が大きい」というフィードバックを受けた。詰まったのは 3 点。

1. **直前のセッションが not found** — 20:44 終了のセッションを 20:47 に `tree <uuid>` で引いたら `"error": "Conversation ... not found"`。手で `index` を叩いて初めて引けた。
2. **`tree` に絞り込みがない** — オプションは `--json` のみ。「ユーザー発言だけ読みたい」のに 265 メッセージのネストした木が丸ごと返り、`jq` で `recurse(.children[]?)` ＋ `message_type=="user"` ＋ `[Tool result]` 除外を手書きする羽目になった。`search` には `--content` / `--content-chars` / `--limit` があるのに `tree` には何もない、という非対称。
3. **task-notification が無標の user メッセージとして全文入る** — 中身は有用なので潰さず、種別タグを付けて呼び出し側が要否を判断できるようにしたい。

加えてスキル本文が UUID 直指定に噛み合っていない（`Level 1: focused search` から始めろとあるが、セッション ID が分かっているなら `search` を経由する意味がない）。

### 調査で判明した、フィードバックとのズレ

- **「自動インデックスが走っていない」は不正確。** `cmd_tree` は既に冒頭で `maybe_background_index()` を呼ぶ（`src/cli.rs:1357`）。効かない理由は 2 つ: 起動するのは**デタッチした別プロセス**なので同一プロセスのクエリには間に合わない（`src/cli.rs:447-459`）／`~/.conversation-search/.last-auto-index` による **300 秒デバウンス**（`src/cli.rs:14-17`, `:419-441`、スタンプは spawn 前に touch: `:443-446`）。構造上「さっき終わったセッション」には間に合わない。
- **そもそも Stop フックが登録されていない。** 利用者の `~/.claude-personal/settings.json` の Stop は `~/.claude/hooks/stop_actions.sh` のみ。`bin/ai-conversation-search setup-hooks` は未実行。つまり自動インデックスは「利用者が検索コマンドを叩いた時に、300 秒デバウンスを超えていれば」しか走っていない。
- **`hooks/hooks.json` は存在するが届いていない。** `hooks/hooks.json:1-15` は `UserPromptSubmit`（スキル誘導リマインダ）だけ。加えて利用者のプラグインキャッシュは `.../conversation-search/0.12.4` と stale で、`hooks/` が入る前のバージョン。フィードバックの `find | grep -i hook` が空だったのはこのため。
- **「0.12.4 なのに `--version` は 0.15.0」の正体。** `/Users/yoshiki.kadono/.local/bin/ai-conversation-search` は手動ビルドのバイナリではなく、**wrapper スクリプトのコピー**（14K、`ACS_WRAPPER_VERSION="0.15.0"` 固定 = `bin/ai-conversation-search:8`）。これが PATH でプラグイン wrapper を隠している。
- **task-notification には一次情報がある。** 当該行は `type:"user"` で `origin:{"kind":"task-notification"}` を持つ。文字列 sniffing 不要。（`promptSource:"system"` も観測されるが全行には無く、Claude Code のバージョン差がある。判定は `origin.kind` のみに依存させる。）
- **`tree --json` は `full_content` を全ノード無条件で出す。** `TreeNode.full_content` は常に serialize（`src/search.rs:192-206`）、一方で人間向け出力は `summary` を 80 字に切るだけ（`src/cli.rs:1394-1406`）。`search` の `--content` オプトイン設計と真逆で、これが「265 メッセージ丸ごと」の原因。

---

## 方針（確定済みの判断）

| 論点 | 採用 | 理由 |
|---|---|---|
| `tree --json` の `full_content` | `search` と同じくオプトイン（**破壊的**） | 文脈消費が最大の問題。互換のために巨大出力を残す価値がない |
| 未インデックス対策 | 同期リトライ **＋** Stop hook 同梱 | 同期リトライが保証、Stop hook が日常的な遅延を潰す |
| task-notification タグ | 新規インデックス分のみ | 遡及は `index --all --force` で利用者が選べる。全再索引の強制はコストに見合わない |

破壊的変更を含むため **0.16.0**（CLAUDE.md「破壊的変更のときだけ minor を上げる」）。

**破壊的変更の影響範囲（調査済み）:** `tree --json` の消費者はリポジトリ内の `bin/ai-conversation-search:223-238`（fzf プレビュー）のみ。`~/.claude-personal` 全体を grep しても、本リポジトリのプラグインキャッシュ／マーケットプレイス複製の中の**ドキュメント記述**しか出てこない。wrapper は `$BINARY`＝バージョン付きキャッシュ（`bin/ai-conversation-search:8-11`）を使うので、wrapper とバイナリのバージョンは常に一致する。

---

## Task 1: `tree` の not-found を同期リトライで回復する

**狙い:** 「さっき終わったセッション」を必ず引けるようにする。実測で最大級の非 observer transcript（18MB / 410 メッセージ）の単一ファイル索引は **0.46 秒**、フル増分索引は 4.7 秒。桁が違うので同期でよい。

### 1-1. トランスクリプト探索を「純関数 + 既存スキャナ」で組む

- [ ] **失敗するテストを先に書く**（`src/indexer/claude_code.rs` の tests に）: `find_session_transcript(&[PathBuf], "deadbeef-....")` が (a) 完全一致で `Some`、(b) 一意なプレフィックスで `Some`、(c) 同一プレフィックス 2 本で `None`、(d) 該当なしで `None` を返す。`cargo test` で落ちることを確認。
- [ ] `pub(crate) fn find_session_transcript(paths: &[PathBuf], session_id: &str) -> Option<PathBuf>` を追加。**引数はスキャン済みのパス列**（環境も DB も触らない純関数なので、`$HOME` を汚さずテストできる）。file stem が完全一致、無ければ prefix 一致が**ちょうど 1 本**の時だけ `Some`。
- [ ] `fn scan_project_dirs`（`src/indexer/claude_code.rs:566`）を `pub(crate)` にする。
- [ ] `cargo test` で通ることを確認。実装を「prefix 一致が 2 本でも `Some` を返す」に戻すと (c) が落ちることを確認（ミューテーション）。
- [ ] コミット。

**重大な落とし穴（レビューで検出）:** `discover_project_dirs`（`src/indexer/claude_code.rs:487-557`）が返すのは `~/.claude*/projects` という**ルート**であって、`.jsonl` が入っているディレクトリではない。実ファイルは `~/.claude/projects/<エンコード済みプロジェクト名>/<uuid>.jsonl` の**2 階層下**（実測: depth 1 に `.jsonl` は 0 個、depth 2 に 15,683 個）。ルートを `read_dir` する実装は必ず空振りする。

→ 自前で 2 階層歩かず、**既存の `scan_project_dirs` をそのまま使う**。これで observer ディレクトリの一括スキップ（`:629-640`）、`agent-*` スキップ（`:648-653`）、summarizer スキップ、mtime ソートを全て継承でき、新しいエッジケースを増やさない。実測コスト **83ms**（114 プロジェクトディレクトリ / 3,818 ファイル、warm）。

日付カットオフは `None`（全期間）を渡す。`Some(7)` にすると「古い未索引セッションだけ引けない」という説明しづらい穴ができるため。

### 1-2. 単一セッション索引ヘルパ（`src/cli.rs`）

- [ ] **失敗するテストを先に書く**: `index_single_session` が `oc:` / `codex:` プレフィックス、8 文字未満、非 hex 文字を含む入力に対して、**DB もファイルシステムも触らずに** `false` を返す。
- [ ] 実装:

```rust
/// Index just the transcript for `session_id`, synchronously, into `db_path`.
///
/// Why not reuse `maybe_background_index`: that path is detached and TTL-debounced, so it
/// can never satisfy a lookup happening in this same process.
///
/// The bool is "one file was handed to the indexer without error", NOT "rows were added":
/// `index_conversation` also returns `Ok(())` for observer, summarizer and empty
/// transcripts (src/indexer/claude_code.rs:1116-1137). The caller only uses it to decide
/// whether a single extra query is worth issuing, so that looseness is fine.
fn index_single_session(db_path: &str, session_id: &str) -> bool {
    // OpenCode / Codex ids do not live in the ~/.claude*/projects/<project>/<uuid>.jsonl layout.
    if session_id.starts_with("oc:") || session_id.starts_with("codex:") {
        return false;
    }
    // Guard the directory sweep: anything that is not a UUID prefix cannot match a
    // transcript filename. Also keeps `tests/test_pick.sh:318`
    // ("definitely-no-such-session-id") from paying for a scan.
    let id = session_id.to_ascii_lowercase();
    if id.len() < 8 || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return false;
    }
    // Never create or migrate a database from a read path: ConversationIndexer::new opens
    // read-write and runs init_schema, so on a missing DB `tree` would silently build one.
    if !std::path::Path::new(&db::expand_path(db_path)).exists() {
        return false;
    }

    let Ok(mut indexer) = ConversationIndexer::new(db_path, true) else {
        return false;
    };
    // A stale claude_code_sync_state row would otherwise short-circuit the re-read
    // (src/indexer/claude_code.rs:1088-1098) and leave the retry a silent no-op — exactly
    // the case this function exists for. One file is sub-second, so force is affordable.
    indexer.set_force(true);

    let dirs = indexer.discover_project_dirs();
    let files = indexer.scan_project_dirs(&dirs, None);
    let Some(path) = find_session_transcript(&files, &id) else {
        return false;
    };
    if let Err(e) = indexer.index_conversation(&path) {
        log::warn!("targeted index of {} failed: {}", path.display(), e);
        return false;
    }
    true
}
```

- [ ] `discover_project_dirs`（`:487`）を `pub(crate)` にする。
- [ ] `cargo test` で通ることを確認 → コミット。

**`db_path` を引数に取る理由:** ハードコードすると `db::DEFAULT_DB_PATH` = `~/.conversation-search/index.db`（`src/db.rs:7`）に対してテストが書き込んでしまう。`858b323` で直したばかりの「環境依存テスト」を再発させない。

### 1-3. `cmd_tree` でリトライする

- [ ] **統合テストを先に書く**（Task 1 の本命、これが無いと 1-1 の空振りを検出できない）: temp dir に `<tmp>/projects/<project>/<uuid>.jsonl` を作り、temp DB を指定して `tree` 相当の経路を呼び、**リトライ後に木が返る**こと。`find_session_transcript` に空の `paths` を渡すよう戻すと落ちることを確認。
- [ ] `src/cli.rs:1356-1359` を差し替える:

```rust
fn cmd_tree(session_id: &str, opts: TreeOpts) -> Result<()> {
    let search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;
    let mut tree = search.get_conversation_tree(session_id)?;

    // `conversation: None` with an error is exactly the two not-found shapes
    // (src/search.rs:1283 unresolved/ambiguous id, src/search.rs:1307 missing conversation
    // row). Matching on the struct rather than the error text keeps this off the message
    // wording. Both raw-transcript fallback outcomes keep `conversation: Some(..)`
    // (src/search.rs:1430-1457), so they never trigger a pointless re-index.
    if tree.conversation.is_none() && tree.error.is_some() {
        drop(search);
        if index_single_session(db::DEFAULT_DB_PATH, session_id) {
            let search = ConversationSearch::new(db::DEFAULT_DB_PATH)?;
            tree = search.get_conversation_tree(session_id)?;
        }
    }

    // Moved below the lookup: leaving it on top made the detached indexer race this
    // command's own synchronous write for the same file, and busy_timeout is 30s
    // (src/db.rs:44). The sync path above already covers the case that matters.
    maybe_background_index();
    ...
}
```

- [ ] 呼び出し側（`src/cli.rs:563`）を新シグネチャに合わせる。
- [ ] `cargo test` → コミット。

**曖昧な ID:** 曖昧も `conversation: None` + `error` だが、`find_session_transcript` が複数一致で `None` を返すのでリトライは走らない。

**observer セッション:** `scan_project_dirs` のディレクトリ名スキップ（`:629-640`）を継承するので、そもそも候補に上がらず not found のまま。仕様どおり。REFERENCE.md の `CONVERSATION_SEARCH_INDEX_OBSERVER` の記述の近くに注記を足す。

**スコープ外:** `context` は message UUID を受けるが、message UUID → ファイルパスの解決手段がインデックス無しでは存在しない。今回は `tree` のみ。

### 1-4. `hooks/hooks.json` に Stop フックを足す

- [ ] `hooks/hooks.json` の `hooks` に追記（既存 `UserPromptSubmit` はそのまま）:

```json
"Stop": [
  {
    "hooks": [
      { "type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}/bin/ai-conversation-search\" hook", "timeout": 5 }
    ]
  }
]
```

- [ ] `cmd_hook`（`src/cli.rs:1432-1435`）に専用 TTL を持たせる。現状は `search`/`tree`/`list` と同じスタンプ（`src/cli.rs:390`, `:443`）を共有するため、セッション中に CLI を 1 回でも叩いていれば Stop が 300 秒デバウンスで no-op になる。`CONVERSATION_SEARCH_HOOK_TTL`（デフォルト 0 = 常に走らせる）を追加し、`hook` だけはセッション終了時に必ず索引させる。
- [ ] README.md の `setup-hooks` 記述（`README.md:59`, `:62`, `:229-232`）と `.claude-plugin/INSTALL.md:22-29` を「プラグイン導入なら自動、手動インストール時のみ `setup-hooks`」に書き換える。
- [ ] CHANGELOG に**二重登録の移行注記**を書く: `setup-hooks` は `settings.json` に文字列 `ai-conversation-search hook` を書き込み、その冪等性チェックはその厳密一致でしか判定しない（`bin/ai-conversation-search:57-84`）。プラグイン側は `"${CLAUDE_PLUGIN_ROOT}/bin/ai-conversation-search" hook` という別文字列なので**検出されず、Stop が 2 回発火する**。実害は索引プロセスが 2 本走ること（WAL なので破損はしない）。既存利用者は settings.json 側を消してよい旨を書く。
- [ ] コミット。

wrapper の fast path（`bin/ai-conversation-search:26-32`）が `hook` をバイナリ未取得でも exit 0 で抜けるので、フック導入がセッション終了をブロックしない。

---

## Task 2: `tree` に絞り込みと content 制御を足す

### 2-1. clap 定義とディスパッチ

- [ ] `src/cli.rs:349-355` の `Tree` を差し替える:

```rust
    Tree {
        /// Session ID
        session_id: String,
        /// Only show messages from this role
        #[arg(long, value_parser = ["user", "assistant"])]
        role: Option<String>,
        /// Drop tool-call and tool-result nodes
        #[arg(long)]
        no_tools: bool,
        /// Return a flat list instead of a nested tree
        #[arg(long)]
        flat: bool,
        /// Include message bodies
        #[arg(long)]
        content: bool,
        /// Max characters of body to show
        #[arg(long, default_value_t = 300, requires = "content")]
        content_chars: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
```

- [ ] 7 引数になるので `struct TreeOpts { role, no_tools, flat, content, content_chars, json }` にまとめ、`cmd_tree(session_id: &str, opts: TreeOpts)` にする。
- [ ] ディスパッチ `src/cli.rs:563` を書き換える。
- [ ] `cargo build` が通ることを確認 → コミット。

`requires = "content"` は `Search`（`src/cli.rs:292`）と同じ扱いに揃える。

**フラグ名を `--no-noise` ではなく `--no-tools` にした理由:** 既存の `summarization::is_tool_noise`（`src/summarization.rs:36`）は「50 文字未満は false」「ツール記法を除いた残りが 100 文字超なら false」という**検索ランキング用**の判定で、短い `[Tool: Read]` ノードを落とさない。置き換える対象の jq（`bin/ai-conversation-search:229-232`）は `startswith("[Tool")` で無条件に落とす。両者は別物なので、`is_tool_noise` を流用せず表示用の述語を明示的に定義する。

### 2-2. フィルタ（`src/cli.rs`、`ConversationTree` の後処理）

- [ ] **失敗するテストを先に書く**（T1-T3、下の表）。
- [ ] 実装:

```rust
/// Display-level noise, deliberately NOT `summarization::is_tool_noise`.
///
/// Why not: that predicate is tuned for search ranking and keeps short `[Tool: Read]`
/// nodes (`content.len() < 50` returns false, src/summarization.rs:48-50). Here the whole
/// point is to drop them, matching the jq filter this replaces in the fzf preview.
fn is_tool_node(node: &TreeNode) -> bool {
    let t = node.full_content.trim_start();
    t.is_empty() || t.starts_with("[Tool") || t.starts_with("[Request interrupted")
}

/// Drop nodes failing `keep`, splicing a dropped node's surviving descendants into its
/// parent's place, and re-pointing their `parent_uuid` at the nearest surviving ancestor.
///
/// Why not drop the whole subtree: a kept reply hanging under a filtered tool result would
/// vanish, which is exactly the content this filter exists to surface. Why rewrite
/// `parent_uuid`: leaving it pointing at a removed node makes the JSON unreconstructable.
fn prune_tree(
    nodes: Vec<TreeNode>,
    surviving_parent: Option<&str>,
    keep: &impl Fn(&TreeNode) -> bool,
) -> Vec<TreeNode> {
    let mut out = Vec::new();
    for mut node in nodes {
        let children = std::mem::take(&mut node.children);
        if keep(&node) {
            node.parent_uuid = surviving_parent.map(str::to_string);
            let uuid = node.message_uuid.clone();
            node.children = prune_tree(children, Some(&uuid), keep);
            out.push(node);
        } else {
            out.extend(prune_tree(children, surviving_parent, keep));
        }
    }
    out
}

/// Depth-first flatten preserving transcript order. `depth` keeps its original value so
/// the caller can still see where a node sat before flattening.
fn flatten_tree(nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    let mut out = Vec::new();
    for mut node in nodes {
        let children = std::mem::take(&mut node.children);
        out.push(node);
        out.extend(flatten_tree(children));
    }
    out
}
```

- [ ] `cargo test` → ミューテーション確認（`out.extend(...)` を `continue` に戻すと T1 が落ちる）→ コミット。

**`depth` について:** 繰り上げ後は `depth` と実際の入れ子が一致しなくなる。転記上の深さとして意味がある値なので保持し、REFERENCE.md に「`--role`/`--no-tools` 使用時、`depth` は元の転記位置を指す」と書く。

### 2-3. `full_content` のオプトイン化（破壊的）＋ 件数

- [ ] **失敗するテストを先に書く**（T4-T6）。
- [ ] `cmd_tree` の JSON 経路に、`inject_full_content`（`src/cli.rs:109-132`）と同じ流儀の後処理を書く。`TreeNode` 構造体も `ConversationTree`（`src/search.rs:180-189`）も変えず、serialize 済みの `serde_json::Value` を再帰的に走査する:
  - `--content` なし → 各ノードから `full_content` キーを削除
  - `--content` あり → `truncate_chars(body, content_chars)`（`src/cli.rs:1017-1021`）で切り、**全ノードに** `full_content_truncated: bool` を書く（`inject_full_content` が常に書くのに合わせる。片方だけ省くと消費者が 2 通りの形を扱う羽目になる）
- [ ] トップレベルに `returned_messages`（絞り込み後のノード数）を注入する。`total_messages` はセッション全体の件数という意味を維持する。
- [ ] 絞り込みの結果 0 件になった場合、`warning` に `"0 of N messages matched the filters"` を設定する。`tree_exit_code`（`src/cli.rs:1348-1354`）は `.error` だけを見るので、絞り込みで空になった木と本当に空の会話が exit 0 で区別できなくなる — その曖昧さを避けるためのコメントが `src/cli.rs:1339-1347` に既にある。`warning` なら exit 0 のまま（部分データありの意味）で意味が通る。
- [ ] 人間向け出力: `print_tree_nodes`（`src/cli.rs:1394-1406`）に `content: Option<usize>` を渡し、`Some(n)` の時は `summary` の下に本文を `n` 字で切ってインデント表示する。`returned_messages` / `warning` も出す。
- [ ] `cargo test` → コミット。

### 2-4. fzf プレビューを新フラグで書き直す

- [ ] `bin/ai-conversation-search:223-238` の jq を置き換える。**ヘッダ 3 行（`project_path` / `total_messages` / 時刻レンジ）は現状のまま残すこと** — メッセージ一覧部分だけが対象:

```sh
PREVIEW_CMD="$BINARY tree {1} --json --no-tools --flat --content --content-chars 150 2>/dev/null | jq -r '...'"
```

- [ ] `tests/test_pick.sh` にメッセージ一覧部分の非対話アサーションを足す（現状 `:287-311` はヘッダ 3 行しか見ておらず、今回書き換える部分は無カバー）。
- [ ] `sh tests/test_pick.sh` が通ることを確認 → コミット。

---

## Task 3: task-notification にタグを付ける

`data/schema.sql:20` の `CHECK(message_type IN ('user','assistant'))` があるため新しい `message_type` は入れられない。既存の `[Tool: X]` / `[Tool result]` プレースホルダと同じ流儀で、本文先頭にタグを付ける。

- [ ] **失敗するテストを先に書く**（T7-T8）。
- [ ] `JsonlEntry`（`src/indexer/claude_code.rs:132-150`）に `origin` を足す。`JsonlEntry` が `Debug` を derive しているので、入れ子の型も `Debug` が要る:

```rust
#[derive(Debug, Deserialize)]
struct EntryOrigin {
    kind: Option<String>,
}
```
`JsonlEntry` のフィールドとして `origin: Option<EntryOrigin>,` を `:148`（`custom_title` の前）に追加。

- [ ] タグ付けを `src/indexer/claude_code.rs:847`（`msg_content` の束縛を閉じる `};`）の**直後**に入れる。`:845` は `match` の腕の閉じ括弧なので、そこに貼ると構文エラーになる:

```rust
            // Tag rather than suppress: the body carries the agent's actual result, which
            // is often the most useful thing in the session. `origin.kind` is first-party
            // metadata on the entry, so this does not sniff the body for a marker string.
            let is_task_notification =
                entry.origin.as_ref().and_then(|o| o.kind.as_deref()) == Some("task-notification");
            let msg_content = if is_task_notification && !msg_content.is_empty() {
                format!("[Task notification] {}", msg_content)
            } else {
                msg_content
            };
```

- [ ] **会話サマリへの漏れを防ぐ**: 直後の `first_user_message` 捕捉（`:849-852`）は先頭 100 文字を取るが、task-notification の本文は `<task-notification>` XML の定型部で始まるため、これが会話サマリになると `list`/`search` の出力が無意味になる。捕捉条件に `&& !is_task_notification` を足す。
- [ ] `is_tool_noise` にはしない（利用者が「潰すのではなくタグ付けが正解」と明示）。
- [ ] `cargo test` → ミューテーション確認（`origin` 判定を外すと T7 が落ち、`!is_task_notification` を外すと T8 が落ちる）→ コミット。

**遡及について:** `claude_code_sync_state` の mtime スキップにより既存 transcript は再パースされない＝既存インデックスは無変更。`index --all --force`（`src/cli.rs:236`）で遡及できる旨を CHANGELOG と REFERENCE.md に書く。

---

## Task 4: ドキュメント

- [ ] **`skills/conversation-search/SKILL.md`**
  - `## Three-Level Search Workflow`（`:118`）の直後に Level 0 を足す:
    > ### Level 0: Session ID Given (SKIP EVERYTHING ELSE)
    > プロンプトにセッション UUID（または 8 文字以上のプレフィックス）が含まれるなら、`search` を経由せず直行する:
    > ```bash
    > ai-conversation-search tree <SESSION_ID> --json --role user --no-tools --flat --content --content-chars 500
    > ```
    > 未インデックスのセッションはこのコマンドが自動で索引して再試行する。手で `index` を叩く必要はない。
  - セクション名を `Search Workflow` に、`:120` の "Execute in order. Do not skip levels." を Level 0 の存在に合わせて直す。
  - todo テンプレート（`:15-22`）に Level 0 の行を足す。
  - 制約（`:24-28`）: **現行テキストは既に "on .jsonl files" と限定されている**ので、禁止範囲の書き換えは不要。追加するのは許可の明示だけ:
    > - Post-processing this tool's own `--json` output with `jq` is fine — but prefer the built-in flags (`--role`, `--no-tools`, `--flat`, `--content-chars`) when they cover the need
  - 最低バージョンを 0.16.0 に上げる（`:76`）。理由も「`tree --json` のデフォルトが `full_content` を含まなくなった／新フラグは古いバイナリでは拒否される」に更新。
  - Context & Tree セクション（`:268-278`）に新フラグと「ユーザー発言だけ読む」レシピを書く。
- [ ] **`skills/conversation-search/REFERENCE.md`**: `:247`（tree の Options / Exit status / observer 注記 / `depth` の意味 / `returned_messages`）と `:420`（JSON Output Format）を更新。
- [ ] **`README.md`**: `:200-206` の tree 記述、`:59`/`:62`/`:229-232` の setup-hooks 記述。
- [ ] コミット。

---

## Task 5: バージョンとリリース

- [ ] `./scripts/bump-version.sh 0.16.0`（Cargo.toml / `.claude-plugin/*.json` / `bin/ai-conversation-search` を一括更新）
- [ ] `CHANGELOG.md` に 0.16.0 を追加。**Breaking** として `tree --json` の `full_content` オプトイン化、移行注記として Stop フック二重登録と `index --all --force` による遡及を明記
- [ ] CLAUDE.md:40-45 のとおり、タグを押す前に `tests/skill-discovery/scenarios.md` のシナリオを 1 つ新規セッションで実行
- [ ] `git tag v0.16.0 && git push origin v0.16.0`

**リリース後の環境作業（リポジトリ外・実行時に再確認すること）:** 本計画に焼き込んだ「キャッシュ 0.12.4」「`~/.local/bin` に wrapper のコピー」は 2026-08-19 時点の観測なので、実行時に再確認する。プラグインを更新し、`~/.local/bin/ai-conversation-search` は削除するか、その `ACS_WRAPPER_VERSION` を書き換える（中身は wrapper スクリプトなので、バージョンを直せば自分でバイナリを取り直す）。これをやらないと今回の修正は届かない。

---

## テスト

全て inline `#[cfg(test)]`（`src/cli.rs:1437`, `src/indexer/claude_code.rs:1654`）。CLI テストのパターンは `src/cli.rs:1483-1497`（in-memory DB + `data/schema.sql` + `ConversationSearch::from_connection`）を踏襲。

| # | 対象 | 検証すること | タスク |
|---|---|---|---|
| T1 | `prune_tree` | 落ちた中間ノードの子が親の位置に繰り上がり、`parent_uuid` が生存する最近傍の祖先を指す | 2-2 |
| T2 | `prune_tree` | role フィルタで assistant ノードだけが消える | 2-2 |
| T3 | `flatten_tree` | 転記順が保たれ、`depth` が維持され、`children` が空になる | 2-2 |
| T4 | tree JSON 後処理 | `--content` なしで全ノードから `full_content` が消える | 2-3 |
| T5 | tree JSON 後処理 | `--content --content-chars N` で切られ、全ノードに `full_content_truncated` が付く | 2-3 |
| T6 | 件数と警告 | `returned_messages` が絞り込み後の件数と一致し、`total_messages` はセッション全体のまま。0 件時に `warning` が付き exit は 0 | 2-3 |
| T7 | task-notification | `origin:{"kind":"task-notification"}` の user 行が `[Task notification] ` 付きで、通常の user 行は無変更 | 3 |
| T8 | task-notification | 先頭が task-notification のセッションで、会話サマリがそれを拾わない | 3 |
| T9 | `find_session_transcript` | 完全一致 / 一意プレフィックス→`Some`、同一プレフィックス 2 本 / 該当なし→`None` | 1-1 |
| T10 | `index_single_session` | `oc:`/`codex:`/8 文字未満/非 hex で、DB もファイルシステムも触らず `false` | 1-2 |
| **T11** | **not-found リトライ（本命）** | temp dir に `<tmp>/projects/<project>/<uuid>.jsonl`、temp DB を用意し、リトライ後に木が返る | 1-3 |

**T11 が必須である理由:** T9/T10 はどちらも「`None`/`false` を返す」ことの検証なので、探索が丸ごと空振りしていても通ってしまう。実際、初稿の計画は `.jsonl` を 1 階層上で探しており、T9/T10 相当のテストでは検出できなかった。**成功経路のテストを最初に書くこと。**

**テストの環境非依存性:** `find_session_transcript` はパス列を引数に取る純関数なので `$HOME` も env も触らない。`index_single_session` のテスト（T10）は早期 return だけを踏むので DB を開かない。T11 は temp DB を明示的に渡す。`CONVERSATION_SEARCH_EXTRA_DIRS` は経由しない（プロセス全体の env を汚す上、`discover_project_dirs` は実ホームの `~/.claude*/projects` も常に走査するため、開発機の transcript 次第で落ちる）。

**ミューテーション確認**（記憶 `feedback_verify_tests_by_mutation`）: 各テストについて実装を戻して落ちることを確認する。特に T11（`find_session_transcript` に空の列を渡すよう戻す）、T1（`out.extend` を消す）、T7（`origin` 判定を外す）。

---

## エンドツーエンド検証

```bash
cargo test && cargo build --release

# 1. 未インデックスセッションの回復 — 別ターミナルで新しい Claude Code セッションを終了させ、
#    その UUID をすぐ渡す。手動 index なしで木が返ること。
./target/release/ai-conversation-search tree <fresh-uuid> --json | jq '.conversation.session_id, .total_messages'

# 2. 絞り込み — 利用者が jq で手書きした内容が 1 コマンドで出ること
./target/release/ai-conversation-search tree <uuid> --json --role user --no-tools --flat --content --content-chars 200 \
  | jq -r '.tree[].full_content'

# 3. デフォルトが軽いこと（full_content が出ないこと）
./target/release/ai-conversation-search tree <uuid> --json | jq '[.. | objects | select(has("full_content"))] | length'   # → 0

# 4. task-notification のタグ — サブエージェントを使った新しいセッションで
./target/release/ai-conversation-search tree <fresh-uuid> --json --content --content-chars 60 \
  | jq -r '.tree[] | select(.full_content | startswith("[Task notification]")) | .full_content'

# 5. Stop hook — プラグイン導入環境でセッション終了後にスタンプが更新されること
ls -l ~/.conversation-search/.last-auto-index

# 6. fzf プレビューが壊れていないこと
sh tests/test_pick.sh && ./bin/ai-conversation-search pick
```
