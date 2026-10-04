# resume-spec — 非起動ハンドオフ

> **For agentic workers:** TDD で1タスクずつ。各ステップは `- [ ]`。

**Goal:** `SESSION_ID` を渡すと起動せずに resume 対象を構造化 JSON で返す {==`resume-spec`==}{>>このコマンド名はこのプロジェクトにはこれが適切? もっといいのある?<<}{#c1} を追加する。agent/Skill が確認→起動の2段階フローを取れるようにする。

**Non-goal:** `resume` 自体の変更、起動処理、MCP 化、`last`/`preview` (別プラン)。

**Architecture:** 既存 `build_resume_command` / `claude_cmd()` / prefix 解決を再利用。新テーブル・再インデックス・スキーマ移行なし。`src/cli.rs` のみの変更 + `REFERENCE.md`。

---

## Context

- `Commands::Resume { uuid }` は message UUID 受け (`src/cli.rs:373-377`)。人間向け文字列のみで JSON 契約なし。
- `inject_resume_command` (`src/cli.rs:1103-1140`) が `search`/`list --json` に `resume_command` を付与。`opencode`/`codex` は `null` (`src/cli.rs:1127-1133`) で「何で開くか」が落ちる。
- `build_resume_command(project_path, session_id, cmd)` (`src/cli.rs:1088-1101`) が shell-safe 判定 + `cd -- ... && claude --resume ...` を組み立て。`is_shell_safe_value` (`src/cli.rs:75-77`) と `shell_quote` (`src/cli.rs:52-62`) が前提。
- `claude_cmd()` (`src/cli.rs:36-38`) は `CC_CONVERSATION_SEARCH_CMD` env または `"claude"`。shell fragment である点に注意 (例: `env FOO=1 claude`)。
- `tree` の session 解決は prefix 一意解決・曖昧エラー・`oc:`/`codex:` 対応 (`src/cli.rs:496-591` の `transcript_lookup_key`/`index_single_session` 周りが規則を持つ)。

## 設計判断

- **`resume_command` は残し、並列物として `resume-spec` を足す。** 既存 `pick` の `cut -f2` パース (`bin/ai-conversation-search:332-333`) と既存 JSON 契約を壊さない。
- **入力は `SESSION_ID` (message UUID ではない)。** `search`/`list` の `session_id` をそのまま渡せる。bare UUID は `tree` と同じ解決規則 (一意 prefix は通す、曖昧はエラー、制御文字・先頭 `-` は拒否)。
- **追加の識別子は導入しない。** `oc:`/`codex:` プレフィクス付き `session_id` で一意のため。
- **`binary`/`args` 分解規則:** `claude_cmd()` を空白分割し、先頭が `env` なら `env`  assignments を除いた実バイナリを `binary` とする。例: `env FOO=1 claude` → `binary: "claude"`。分解不能でも `resume_command` 文字列はそのまま返す (eval 互換維持)。

---

## CLI 形状

```bash
ai-conversation-search resume-spec SESSION_ID [--json]
```

- `SESSION_ID`: `search`/`list` の `session_id`。`oc:`/`codex:` 付き可、bare UUID の一意 prefix 可。
- `--json` なし=人間向け4行、あり=下記 JSON。

### 出力スキーマ (JSON)

```json
{
  "source": "claude_code",
  "session_id": "abc-123",
  "project_path": "/home/user/proj",
  "project_exists": true,
  "binary": "claude",
  "args": ["--resume", "abc-123"],
  "resume_command": "cd -- /home/user/proj && claude --resume abc-123"
}
```

- `source: opencode/codex` → `binary: null, args: [], resume_command: null` + `note: "resumed with their own tools"`。現状の `inject_resume_command` と一致。
- shell-safe でない (`is_shell_safe_value` false / 先頭 `-`) → `resume_command: null` + `error: "unsafe_path"` (または `"unsafe_session_id"`)。`project_exists` はそのまま返す。
- `project_path: null` → `project_exists: null`、`resume_command: null` + `error: "no_project_path"`。
- 曖昧 prefix → exit 1。文言は `tree` と同じ (`N matches` を含む)。

---

## Task 1: 解決ヘルパの共通化

**Files:** `src/cli.rs`

- [ ] **Step 1: `resolve_resume_target(input) -> Result<ResumeTarget>` を新設。** `ResumeTarget { source, session_id, project_path }`。message UUID と session ID の両方を受け、DB 引きで正規化する。`oc:`/`codex:` は保持。bare UUID は前方一致で一意解決。
- [ ] **Step 2: 既存 `cmd_resume` をヘルパ経由に付け替え。** 振る舞い変更なし (`cargo test` 緑で確認)。
- [ ] **Step 3: コンパイル確認。** Run: `cargo build`。Expected: 成功。

## Task 2: `resume-spec` 本体

**Files:** `src/cli.rs`

- [ ] **Step 1: `Commands::ResumeSpec { session_id: String, json: bool }` を追加。**
- [ ] **Step 2: `cmd_resume_spec(&session_id, json)` を実装。** JSON 構築は `build_resume_command` を再利用。`binary`/`args` 分解、`project_exists` (`Path::exists()` 1 stat)、`note`/`error` 付与。
- [ ] **Step 3: 人間向け表示。** `--json` なしは:
  ```
  source: claude_code
  cwd: /home/user/proj (exists)
  run: claude --resume abc-123 (in /home/user/proj)
  eval: cd -- /home/user/proj && claude --resume abc-123
  ```
  `resume_command == null` の場合は理由行 (`OpenCode sessions are resumed with their own tools` 等) を出す。
- [ ] **Step 4: テスト (Rust):**
  - claude 行 → `binary == "claude"`、`args == ["--resume", id]`、`project_exists` が bool
  - `oc:xxx` → `resume_command is null` かつ `binary is null`
  - 制御文字入り path → `resume_command is null` + `error == "unsafe_path"`、パニックなし
  - 曖昧 prefix → Err に `matches` 件数を含む
- [ ] **Step 5: `cargo test cmd_resume_spec` PASS。**

## Task 3: ドキュメント

**Files:** `skills/conversation-search/REFERENCE.md`、`README.md`、`CHANGELOG.md`

- [ ] **Step 1: REFERENCE.md に `resume-spec` 節。** JSON 例 + `resume_command == null` の2条件 (opencode/codex、shell-safe でない) を `search`/`list` と同じ文言で。
- [ ] **Step 2: SKILL.md ワークフローに1行。** `preview` → `resume-spec` 提示 → 確認後に `resume`。確認なし起動の禁止。
- [ ] **Step 3: CHANGELOG Unreleased に追加のみとして記載** (破壊なし)。

---

## Verification

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build --release
./target/release/ai-conversation-search resume-spec <SESSION_ID> --json | jq .binary
./target/release/ai-conversation-search resume-spec oc:<ID> --json | jq .resume_command  # → null
```

期待値: claude 行で `binary` が取れる、`oc:`/`codex:` で `null` + `note`、既存 `resume`/`search`/`list` の出力不変。

---
comments:
  c1:
    by: user
    at: 2026-10-04T11:59:21.590Z
    resolved: false
---
