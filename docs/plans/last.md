# last — 最新セッションに直帰

> **For agentic workers:** TDD で1タスクずつ。各ステップは `- [ ]`。

**Goal:** 「直前の作業に戻りたい」を1コマンドにする `last` を追加する。`eval "$(ai-conversation-search last)"` で復帰できる。

**Non-goal:** 起動処理自体の変更、fzf UI、`preview` (別プラン)。`resume-spec` プランが先行している前提 (dry-run 出力として再利用)。

**Architecture:** `list` 経路 (`SearchFilter { sort: Recent }`) の再利用。新 SQL なし。`src/cli.rs` のみ + README。

---

## Context

- `Commands::List` は `SearchFilter { sort: Recent }` 固定 (`src/cli.rs:670-692`)。`last` は `limit: 1` の `list` と等価。
- `pick --here` はカレント限定 + git-common-dir での repo 推定 (`bin/ai-conversation-search:166-174`)。Rust 側に同等物はない。
- `--exclude-project` / `--exclude-repo` は未実装。observer ノイズ除けに必要（所有権は `list-enrich.md`。本プランは使う側）。
- `resume-spec` プラン (別ファイル `resume-spec.md`) の JSON を `--dry-run/--json` 出力として使う。

## 設計判断

- **新規 SQL を書かない。** `limit: 20` で引いて Rust 側で除外・先頭取得。除外を SQL `NOT LIKE` にすると複数条件・部分一致が煩雑になるため。
- **`--here` は `getcwd` prefix 比較。** `git2` 依存を増やさない。`project_path.startswith(cwd)` を第一規則、`repo_root` フォールバックを第二規則とする。`pick --here` の worktree 解決と挙動差が出る場合は README に明記。
- **既定出力は eval 可能な1行 resume コマンド。** `pick` の `eval "$(... pick)"` と対称。

---

## CLI 形状

```bash
ai-conversation-search last [--here] [--exclude-project STR]... [--exclude-repo STR]... [--source SRC] [-n|--dry-run] [--json]
```

- `--here`: カレントディレクトリ配下のみ（prefix 一致）。
- `--exclude-project STR` / `--exclude-repo STR`: 部分一致除外。複数可（繰り返し指定）。ヘルパは `list-enrich.md` の `matches_exclude_project` / `matches_exclude_repo` を使う。
- `--source`: `claude_code`/`opencode`/`codex` (既存と同じ `value_parser`)。
- `-n`/`--dry-run`: 起動せず `resume-spec` JSON だけ出す。
- `--json`: 同 JSON を出す (`-n` と同形。両方あれば JSON)。

---

## Task 1: `last` 本体

**Files:** `src/cli.rs`

- [ ] **Step 1: `Commands::Last { here: bool, exclude_project: Vec<String>, exclude_repo: Vec<String>, source: Option<String>, dry_run: bool, json: bool }` を追加。** `-n` は `dry_run` の short (`#[arg(short = 'n')]`)。
- [ ] **Step 2: `cmd_last(...)` を実装。** `SearchFilter { days_back: None, limit: 20, sort: Recent, source, .. }` で `list` 相当を取得 → `matches_exclude_project` / `matches_exclude_repo` で除外 → 先頭1件。0件なら exit 1 + `No sessions found` (+ フィルタの緩め方のヒント)。
- [ ] **Step 3: 除外ヘルパは `list-enrich.md` のものを使う。** 本プランでは新設しない（所有権は `list-enrich`）。
- [ ] **Step 4: `--here` 解決。** `std::env::current_dir()` と `project_path` の prefix 比較。`project_path` NULL 行は `--here` 指定時は落とす。
- [ ] **Step 5: 出力分岐。** `dry_run || json` → `resume-spec` と同じ JSON を表示。既定 → `resume_command` 1行を stdout。`resume_command == null` 行が先頭に来たら次の候補に進むか、なければ exit 1 + 理由 (OC/CX 行は自ツールで開く旨)。
- [ ] **Step 6: テスト:**
  - 空 DB → exit 1 + `No sessions found`
  - `--exclude-project` で唯一候補が消える → 同じく 0件
  - `--here` で他ディレクトリのみ → 0件、カレント配下あり → それが返る
  - 除外ヘルパの単体テストは `list-enrich` 側に置く（本プランでは結合テストのみ）

## Task 2: ドキュメント

**Files:** `README.md`、`skills/conversation-search/REFERENCE.md`、`CHANGELOG.md`

- [ ] **Step 1: README Command Reference に `last`。** `eval "$(ai-conversation-search last)"` と `eval "$(ai-conversation-search pick)"` を並記。`--here` の worktree 注意があれば注記。
- [ ] **Step 2: REFERENCE.md に完全リファレンス。** `--exclude-project` / `--exclude-repo` の部分一致仕様を明記。
- [ ] **Step 3: CHANGELOG Unreleased に追加として記載** (破壊なし)。

---

## Verification

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build --release
./target/release/ai-conversation-search last --dry-run --json | jq .session_id
./target/release/ai-conversation-search last --here --dry-run --json | jq .project_path
# 要ユーザー確認の上で: eval "$(./target/release/ai-conversation-search last --here)"
```

期待値: 最新1件が返る、`--exclude-project`/`--exclude-repo`/`--here` で絞れる、既存 `list` の出力不変。
