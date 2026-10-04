# preview — 単体プレビュー + ハイライト

> **For agentic workers:** TDD で1タスクずつ。各ステップは `- [ ]`。

**Goal:** `pick` 内部に埋め込まれたプレビューを独立サブコマンド `preview` として切り出す。`--query` ハイライトと `--json` で agent/人間の両方が使える read-only 閲覧経路にする。

**Non-goal:** 検索ランキング変更、起動処理、fzf モード切替自体 (別プラン `pick-modes.md`)、SQL LIMIT による取得最適化 (将来)。

**Architecture:** 既存 `tree --flat --no-tools --content --json` と同一エンベロープに `query` / `matches` (+ `project_exists`) だけ足す薄い束ね。新規クエリ・新テーブルなし。再利用: `lookup_tree` → `filter_tree(no_tools+flat)` → 末尾 N 件 → `apply_tree_content` → `localize_timestamps`。`src/cli.rs` (+ 既存 tree 取得関数の再利用) と `bin/ai-conversation-search` の preview 差し替え。

---

## Context

- `tree` は `--role/--no-tools/--flat/--content/--content-chars/--json` を持つ (`src/cli.rs:350-372`)。`total_messages` と `returned_messages` の区別、`--content` opt-in 契約がある (`REFERENCE.md` tree 節)。
- `tree --flat --no-tools --content --json` の実体は `{ conversation: {..., session_id/source/project_path/first_message_at/last_message_at}, tree: [{message_uuid/session_id/parent_uuid/depth/timestamp/message_type/project_path/summary/full_content/full_content_truncated/children/is_sidechain}], total_messages, returned_messages, warning?/error? }`。`localize_timestamps` で時刻はローカル化される。`preview --json` はこの包みを変えない (後述)。
- `truncate_chars` は `chars()` 基準でマルチバイト安全 (`src/cli.rs:1166-1170`)。ハイライトも同じ基準にする必要がある。
- `pick` の reload は `search --group-by-session --json` (`bin/ai-conversation-search:246-272`)、preview は `{1}` = session_id 列 (`bin/ai-conversation-search:319-328`)。
- 依存順序: 本プラン③は① `resume-spec.md` Task 1 の `resolve_resume_target` の後に着手する (`session-ux-index.md` の依存関係通り)。①未完の間は着手しない (解決ロジックの複製を作らない)。

## 設計判断

- **`tree` の薄い束ねに留める。** 新しい本文取得経路を作らず、flat + no-tools + content の最終 N 件を取るだけ。SQL LIMIT 化は今回なし (長大セッションでの無駄は将来最適化、Non-goal に明記)。
- **JSON は `tree` 互換 (superset) にする。独自 `messages` 包みは作らない。** フラット独自形式 (`{session_id/source/project_path/messages: [...]}`) をやめた理由: (1) 既存の `tree --json` 読み手 (fzf preview の jq、agent のパーサ、SKILL) がそのまま読める、(2) 実装が `apply_tree_content` / `localize_timestamps` の再利用になり分岐が減る、(3) 差分がトップレベルの `query` / `matches` (+ `project_exists`) 追加だけになりレビューが容易。`tree[]` ノード形状・`full_content` + `full_content_truncated` 契約・`total/returned` 意味は `tree` と同一。
- **`project_exists` のみ additive 追加を許容。** `tree` 包みにはないが `resume-spec` と同じ `Path::exists()` 1 stat でトップレベルに付ける。`--json` / 人間表示ともに `resume-spec` と同じ判定を使う。
- **既定 N=30。** env による上書きはなし (YAGNI — `pick` 内からは `--messages 12` を明示して呼ぶ。fzf ペイン幅のため現状の `[-12:]` と等価)。`--messages` / `--content-chars` は `tree` と同じ `range(1..)` で 0 を拒否する。
- **`matches` は返却 N 件内のみ。件数は変えない。** `--query` はハイライト/`matches` 抽出専用でフィルタしない。`matches` 判定は切り詰め前 full 本文に対する部分一致 (表示は切り詰め後にハイライト)。順序は truncate → highlight。ANSI コードは `content-chars` に含めない。
- **`--query` は単一フレーズ、前後 trim、空は無視。** 大文字小文字無視は ASCII-only (日本語は素通し)。複数語の AND/OR 分割はしない — `pick-modes.md` の grep モード (`--query {q}`、fzf の `{q}` はスペース入り得る) との契約は「フレーズ1つ」とし、分割が必要になったら別プランで扱う。
- **ハイライトは人間向けのみ。** `--json` では該当 `message_uuid` 一覧 (`matches`) を返す。ANSI を JSON に混ぜない (`--no-color` の有無に関わらず)。
- **ヘッダはプレーン形式。** `Project:/Messages:/Range:` で出す。例:
  ```
  Project: /Users/.../meetsone
  Messages: 91-120/120 (returned/total)
  Range: 2026-09-01 10:00 → 2026-09-01 12:00
  ```
  `Messages` は返却範囲インデックス + `returned/total`、`Range` は返却 N 件の `first→last` (ローカル表示、`tree` の `first/last_message_at` と同値)。

---

## CLI 形状

```bash
ai-conversation-search preview SESSION_ID [--query Q] [--messages N] [--json] [--no-color] [--content-chars N]
```

- `SESSION_ID`: `resume-spec` Task 1 の `resolve_resume_target` と同じ解決 (bare prefix 可、`oc:`/`codex:` 可、曖昧は exit 1 + `N matches` 文言は `tree` と同一)。本プランは①の後に着手するためヘルパを再利用し、複製しない。
- `--messages N`: 末尾 N 件 (既定 30、`range(1..)`)。`total_messages` はセッション全体件数、`returned_messages` は返却件数 (`tree` と同一意味)。
- `--query Q`: 単一フレーズ部分一致 (ASCII 大文字小文字無視、前後 trim、空は無視) でハイライト/`matches` 抽出。件数は変えない。
- `--no-color`: ANSI 無効。`NO_COLOR` env・非TTY でも自動無効 (`std::io::IsTerminal` 判定)。`--json` では常に ANSI なし。`CLICOLOR_FORCE=1` でパイプ時も強制発色 (fzf プレビューペイン用 — 将来の grep モードが `--query {q}` と併用する。`NO_COLOR` が両立設定時は `NO_COLOR` が勝つ)。
- `--content-chars N`: 既定 300 (`tree` と同じ、`range(1..)`)。`preview` では常時 content 扱いのため `requires = "content"` 制約は付けない。

### 出力スキーマ (JSON) — `tree --json` の superset

`tree --flat --no-tools --content --json` と同一包みに `query` / `matches` / `project_exists` を足すだけ。`messages` という新キーは作らない。`tree[]` 配列が末尾 N 件になる点だけが `tree` との差分。

```json
{
  "conversation": {
    "session_id": "...",
    "source": "claude_code",
    "project_path": "...",
    "first_message_at": "2026-09-01T10:00:00+09:00",
    "last_message_at": "2026-09-01T12:00:00+09:00"
  },
  "total_messages": 120,
  "returned_messages": 30,
  "tree": [
    {
      "message_uuid": "...",
      "message_type": "user",
      "timestamp": "2026-09-01T11:59:00+09:00",
      "full_content": "...",
      "full_content_truncated": false,
      "children": [],
      "depth": 0
    }
  ],
  "project_exists": true,
  "query": "auth",
  "matches": ["msg-uuid-1"]
}
```

- `query` なし/空 → `"query": null, "matches": []`。
- `matches` は返却 `tree[]` 内で query を含む行の `message_uuid` のみ (全体検索ではない)。判定は切り詰め前 full 本文。
- 存在しない id・曖昧 prefix → exit 1。`--json` でもエラー包みは `tree` と同じ (`error` キー + stderr) にし、新形状のエラー包みは作らない。

---

## Task 1: `preview` 本体

**Files:** `src/cli.rs`

- [ ] **Step 1: `Commands::Preview { session_id, query, messages, json, no_color, content_chars }` を追加。** `messages` 既定 30、`content_chars` 既定 300、いずれも `range(1..)`。`content_chars` に `requires = "content"` は付けない (`preview` は常時 content)。
- [ ] **Step 2: `cmd_preview(...)` を実装。** `resolve_resume_target` (①のヘルパ) で正規化 → `lookup_tree` → `filter_tree(no_tools=true, flat=true)` → 時刻昇順のまま末尾 N 件 (`flatten_tree` 済みなので `vec.split_off(len - min(N, len))`) → `total/returned` 付与。人間向けヘッダは `Project:/Messages: <start>-<end>/<total> (returned/total)/Range: <first> → <last>` のプレーン形式。`--json` なしの本文表示は `print_tree_nodes` 相当 + ハイライト。
- [ ] **Step 3: ハイライト関数 `highlight_query(line, query) -> String`。** ASCII 大文字小文字無視の部分一致を ANSI bold/red で囲む。複数ヒットは全件囲む。`chars()` 基準でバイト境界パニックなし (lower 後の byte index をそのまま使わない — `to_lowercase` で検索して元文字列の char 境界にマップする)。`no_color`/`NO_COLOR`/非TTY/`--json` で素通し。判定順序: truncate (切り詰め前 full で `matches` 判定) → highlight (切り詰め後に表示だけ)。
- [ ] **Step 4: `--json` 組み立て。** `serde_json::to_value(&tree)` → `localize_timestamps` → `apply_tree_content(content=true)` の順は `cmd_tree` と同一 → トップレベルに `query` (なし/空は null)、`matches` (返却 `tree[]` 内の含有 uuid のみ)、`project_exists` (`resume-spec` と同じ `Path::exists()` 1 stat) を追加するだけ。`full_content` + `full_content_truncated` 契約は `tree` と同一にする (欠落させない)。
- [ ] **Step 5: テスト:**
  - 存在しない id → exit 1 (`tree` と同じ文言)。`--json` でも `error` キー形状は `tree` と同一
  - 曖昧 prefix → exit 1 + `matches` 件数を含む文言 (`tree` と同一)
  - `--messages 5` で末尾5件のみ、`returned_messages == 5`、`total_messages` は全体件数のまま
  - `--query` なしでも動く (`query: null, matches: []`)、あっても件数 (`tree[].len()`) が変わらない (ハイライトのみ)
  - `matches` は返却 N 件内の uuid の部分集合である (全体件数の uuid を含まない)
  - ハイライト単体テスト: ASCII 大文字小文字無視、複数ヒット、日本語境界でパニックなし
  - `--json` に `full_content` + `full_content_truncated` が全ノードに入る、ANSI を含まない
  - `tree --flat --no-tools --content --json` の末尾 N 件と `preview --messages N --json` の `tree[]` が一致する (互換テスト)

## Task 2: `pick` の preview 差し替え

**Files:** `bin/ai-conversation-search`

- [ ] **Step 1: `PREVIEW_CMD` (`bin/ai-conversation-search:227-238` の `tree | jq`) を `preview {1} --messages 12 --content-chars 150` 相当に置き換え。** grep モード (別プラン) からは `--query {q}` を足せる形に変数化しておく (`ACS_PREVIEW_BASE="preview {1} --messages 12 --content-chars 150"` + grep 時のみ `--query {q}` 付与。fzf の `{q}` 置換は shell-quoted される前提だが、空クエリ時は `--query` を付けない分岐にする)。人間向け表示が `📁 💬 🕐` 絵文字からプレーン header に変わるため、目視で同等性を確認する。
- [ ] **Step 2: `tests/test_pick.sh` が緑 + preview アサーション更新。** CLAUDE.md 手順で実機実行 (`ACS_TEST_BINARY="$PWD/target/release/ai-conversation-search" sh tests/test_pick.sh`)。現行の `🕐` を grep する preview アサーションは新 header (`Project:/Messages:/Range:`) に合わせて更新する (絵文字依存を残さない)。

## Task 3: ドキュメント

**Files:** `skills/conversation-search/REFERENCE.md`、`skills/conversation-search/SKILL.md`、`CHANGELOG.md`

- [ ] **Step 1: REFERENCE.md に `preview` 節 + JSON 例。** `tree --json` との差分は `query` / `matches` / `project_exists` 追加 + `tree[]` が末尾 N 件である点だけと明記する。`messages` キーは存在しないことを書く (旧案との混同防止)。
- [ ] **Step 2: SKILL.md の read-first フローに `preview --query` を明記。** `search` で絞れた後の1手として `preview --messages 12 --query <phrase>` → 必要なら `resume-spec` で確認後に `resume` の順にする。
- [ ] **Step 3: CHANGELOG Unreleased に追加として記載。** (README への追記は今回なし — 既存コマンド一覧の体裁に合わせる必要があれば別途)。

---

## Verification

```bash
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build --release
./target/release/ai-conversation-search preview <SESSION_ID> --query auth | head -n 20
./target/release/ai-conversation-search preview <SESSION_ID> --json | jq '{returned: .returned_messages, total: .total_messages, matches: .matches, keys: keys}'
./target/release/ai-conversation-search tree <SESSION_ID> --flat --no-tools --content --content-chars 300 --json | jq '.tree | length'
ACS_TEST_BINARY="$PWD/target/release/ai-conversation-search" sh tests/test_pick.sh
```

期待値: 末尾 N 件が返る、query でハイライト/`matches` が付く、`tree --json` と包みが一致する (`tree[]` 長さが末尾 N と一致、`matches` は返却内の部分集合)、`pick` の preview が新経路で動く。
