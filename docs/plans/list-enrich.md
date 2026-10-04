# list 強化 — project_exists/project_basename + exclude-project/exclude-repo + list --project/--here

> **For agentic workers:** TDD で1タスクずつ。各ステップは `- [ ]`。

**Goal:** `list`/`search --json` の各行を agent 向けに強化する。`project_exists`/`project_basename` を返し、`--exclude-project`/`--exclude-repo` でノイズ除けできるようにする。`list` に欠けている `--project`/`--here` を足し、「今いるプロジェクトの最近」を1コマンドにする。

**Non-goal:** ランキング変更、起動処理、config ファイル。本プランが `matches_exclude` 系ヘルパを作り、`last` が使う（所有権は本プラン）。

**Architecture:** `inject_resume_command` と同じ箇所 (`src/cli.rs:1103-1140`) でフィールド付与。除外は SQL ではなく Rust 側フィルタ。新テーブルなし。

---

## 用語集（本プラン以降これで固定。`cwd`/`dir` は増やさない）

- `project_path` = セッションが起動した時にいたディレクトリ（DB値。worktreeごとに違う: `.../meetsone` vs `.../meetsone.worktrees/claude`）
- `repo_root` = `git --git-common-dir` 由来のメインリポジトリルート（DB値。worktree横断で同一。`src/git_utils.rs:6`）
- `cwd` = クエリ実行時の今いる場所（`--here` の比較元にのみ使う語。DB値ではない）
- `basename` = 表示用末尾1段

旧名 `cwd_exists`/`cwd_basename`/`--exclude-dir` は使わない。値の出所で名付ける（`project_*` / `repo_*`）。`resume-spec`（PR #6）/`preview`（PR #7）は main にマージ済みだが Unreleased のため、本プラン実施時に `cwd_exists` → `project_exists` へ改名する（リリース済み 0.17.0 には含まれないので互換エイリアスなし）。`resume-spec.md`/`preview.md`/`last.md` 内の `cwd_exists` 言及も同読み替え済み。

---

## Context

- JSON envelope は `search`/`search --group-by-session`/`list` で共通 (`JsonEnvelope`、`src/cli.rs:89-92`)。`print_json_envelope` (`src/cli.rs:142-155`) で `inject_resume_command` 後に包む。
- `SearchResultRow` は `project_path/source/session_id` を持つ (`src/search.rs:50-69`)。`ConversationRow` は `project_path` + `repo_root` を持つ (`src/search.rs:91-106`)。
- 現状の絞り込み: `search` は `--project`（完全一致 `= ?`）+ `--repo`（部分一致 `LIKE %..%`）。`list` は `--repo` のみで `--project` がない。`pick --here` は `$(pwd)` exact + `git-common-dir` basename の AND で狭すぎる。
- 除外フラグは未実装。`last` プランが本プランのヘルパを使う依存関係。

## 設計判断

- **追加のみ、破壊なし。** 既存フィールドを変えず `project_exists`/`project_basename` を足す（`resume-spec` 作業ツリー分の改名を除く）。
- **除外系はどちらも部分一致。** `--project`（完全一致）と非対称だが、ノイズ除け（`observer`、`/tmp`）には部分一致が必要なため。case は SQLite `LIKE` 通り。大文字小文字・trailing slash の正規化はしないことを REFERENCE に明記。
- **`--here` は prefix 一致に統一。** `project_path.startswith(canonicalized cwd)`。サブディレクトリから叩いても拾える。`project_path: null` 行は `--here` 指定時は落とす。`pick --here` の exact AND は本プラン範囲外（将来寄せる。非ゴールに明記）。
- **除外は Rust 側。** SQL `NOT LIKE` にしない（複数・部分一致のため）。`limit` との関係は埋まるまで追い fetch し、`truncated` 契約を維持する（下記 Step 4）。
- **`basename` は `Path::file_name()`。** `/` 分割の手実装にしない（trailing slash・Unicode 対策）。空・取得不可 → `null`。
- **SKILL.md も同梱。** read-first フロー（`list/search --json --limit 5` → 3-5件提示 → `preview` → `resume-spec` → 確認後 resume）を明文化する。

---

## Task 1: JSON 強化（project_exists/project_basename）

**Files:** `src/cli.rs`、`src/search.rs`

- [ ] **Step 1: `inject_resume_command` と同箇所で `project_exists: bool|null` + `project_basename: string|null` を付与。** `project_path: null` → 両方 `null`。`project_basename` は `Path::file_name()` の末尾（空・不可なら `null`）。`resume-spec`/`preview` の `cwd_exists` 改名は先行実施済み。
- [ ] **Step 2: 人間向け `list`/`search` 表示にも反映。** `project_basename` 表示 + `project_exists == false` 行に `(missing)` マーカー。JSON のみの強化にしない。
- [ ] **Step 3: テスト:**
  - `--json` 行に `project_exists`/`project_basename` が入る（snapshot 的 assert）
  - `project_path: null` 行 → 両方 `null`、パニックなし
  - trailing slash・日本語パスでパニックなし、`basename` が末尾になる
  - 既存 `rowid` 非露出テスト（`test_json_output_has_no_rowid_field` 相当）が緑のまま

## Task 2: 除外フラグ + list --project/--here

**Files:** `src/cli.rs`、`src/search.rs`

- [ ] **Step 1: `matches_exclude_project(path: Option<&str>, excludes: &[String]) -> bool` と `matches_exclude_repo(repo_root: Option<&str>, excludes: &[String]) -> bool` を新設**（所有権は本プラン。`last` が使う。空は false、部分一致。2つとも同じ実装の薄いラッパでよい）。
- [ ] **Step 2: `search`/`list` に `--exclude-project STR`（複数可、繰り返し指定）+ `--exclude-repo STR`（同）を追加。** `SearchFilter` には持たせず CLI 側で取得後にフィルタ（SQL 変更なし）。いずれかの一致で除外（OR）。
- [ ] **Step 3: `list` に `--project STR`（`search` と同じ完全一致）と `--here`（prefix 一致）を追加。** `search` の `--project` と意味を合わせる。`--here` は `std::env::current_dir()` を正規化して `project_path.startswith(cwd)` で判定。
- [ ] **Step 4: `limit` との詰め物規則。** `limit: N` で引いて除外したら N 件未満になるため、埋まる（または枯渇する）まで LIMIT を伸ばして追い fetch する。ただし1回の FTS が数秒かかる大インデックスで無限ループにしないため上限あり: 最大3クエリ、伸びは ×4、1回の上限1000件。3回で埋まらなければ部分結果 + `truncated: true`（保守的）。`truncated` は「まだ奥に未取得行がある場合 true」に再計算する。除外のみで枯渇したら `truncated: false`。
- [ ] **Step 5: テスト:**
  - `--exclude-project observer` で該当行のみ消える、無関係行は残る、複数指定の OR
  - `--exclude-repo` が `repo_root` 側に効く（`project_path` のみ一致では消えない）
  - `list --project <exact>` で完全一致のみ、`list --here` で配下のみ、`project_path: null` 行は `--here` で落ちる
  - 除外で減った分が追い fetch で埋まる + `truncated` が正しい（除外のみ枯渇 → false）

## Task 3: Skill ワークフロー更新

**Files:** `skills/conversation-search/SKILL.md`、`skills/conversation-search/REFERENCE.md`

- [ ] **Step 1: SKILL.md に read-first を明文化。** `list/search --json --limit 5` → 候補3-5件に絞って提示 → `preview --query` → `resume-spec` 提示 → 確認後に `resume`/`eval`。確認なし起動の禁止。非TTY の agent は `--dry-run`/`resume-spec` 提示に留める。定型に `--exclude-project observer` 例と `select(.project_exists != false)` の jq 雛形、`truncated == true` 時の引き上げ手順を添える。
- [ ] **Step 2: REFERENCE.md に `project_exists`/`project_basename`/`--exclude-project`/`--exclude-repo`/`list --project`/`--here` を追記。** `resume_command == null` の扱い、用語集（`project_path`/`repo_root`/`cwd`/`basename`）、除外系は部分一致・正規化なし、`--here` は prefix 一致、とセットで。
- [ ] **Step 3: CHANGELOG Unreleased に追加のみとして記載。**

---

## Verification

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build --release
./target/release/ai-conversation-search list --json --limit 2 | jq '.results[0] | {project_exists, project_basename}'
./target/release/ai-conversation-search list --exclude-project observer --json | jq '.results | length'
./target/release/ai-conversation-search list --exclude-project tmp --exclude-repo meetsone --json | jq '.results | length'
./target/release/ai-conversation-search list --project "$(pwd)" --json | jq '.results[0].project_path'
./target/release/ai-conversation-search list --here --json | jq '.results[0].project_path'
```

期待値: 既存フィールド不変で2フィールド追加、除外が効く、`list --project`/`--here` で今いるプロジェクトに絞れる、`last` プランのヘルパとして使える。

---

## Ripple（別プランへの波及。実装時に反映）

- `resume-spec.md` / `preview.md` / `last.md` 内の `cwd_exists` → `project_exists` に読み替え済み。`src/cli.rs` の改名も実施済み（本作業ツリー）。
- `last.md` の `matches_exclude(path, excludes)` → 本プランの `matches_exclude_project` / `matches_exclude_repo` を使う形に付け替え。`--exclude-dir` → `--exclude-project` / `--exclude-repo` に。
- `pick --here` の exact AND → prefix 一致への寄せは本プラン対象外（将来プラン）。
