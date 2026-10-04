# pick 3モード切替 — fuzzy / dir / grep

> **For agentic workers:** シェル変更のみ。各ステップは `- [ ]`。

**Goal:** `pick` に3モード切替を載せる。打鍵中に fuzzy (全体) / dir (ディレクトリのみ) / grep (本文行単位+ハイライト) を切り替えられるようにする。

**Non-goal:** Rust 変更、検索ランキング変更、config ファイル対応 (フォローアップ)。`preview`/`resume-spec` 本体は別プラン (本プランはそれらを呼ぶ側)。

**Architecture:** `bin/ai-conversation-search` の `acs_pick` 関数のみ変更。fzf の `change-nth` / `rebind` / `transform` を使う。Rust 側は `search`/`preview` の既存 flags を呼ぶだけ。

---

## Context

- 現状 `pick` は `--disabled + change:reload(search --group-by-session)` の1モード (`bin/ai-conversation-search:319-328`)。reload スクリプトは `bin/ai-conversation-search:246-272`、preview は `bin/ai-conversation-search:227-238`。
- `--days/--repo/--here/--source` は起動時固定 (`bin/ai-conversation-search:154-204`、`ACS_*` export で reload に引き継ぎ)。
- `--help` は `bin/ai-conversation-search:177-200`。
- ラッパーはバージョン pin (`bin/ai-conversation-search:10-12`) のため、古いバイナリにない flags を無条件で渡すと壊れる。前プランの `--sort` 対応と同様、新 flags は存在確認か引き継ぎ対象外にする。

## 設計判断

- **Rust 無変更。** モード差は reload クエリと fzf `nth` で表現する。再クエリが必要なのは grep のみ。
- **既定キー:** `ctrl-f` fuzzy / `ctrl-o` dir / `ctrl-g` grep。ヘッダは解決済みキーから動的生成 (ハードコードしない)。
- **config ファイルは今回なし。** 解決順は CLI flags > env (`ACS_PICK_BIND_FUZZY/DIR/GREP`) > defaults。config はフォローアップ (`~/.config/ai-conversation-search/config.toml` 候補)。
- **重複・予約語は fail-fast。** 3キー重複、`enter`/`change` 等の予約語は起動エラー。

---

## モード定義

| モード | reload | fzf マッチ | preview |
|---|---|---|---|
| `fuzzy` (既定) | `search "$q" --group-by-session --json` (現状維持) | `nth=3..` 全体に fuzzy | `preview {1} --messages 12` |
| `dir` | 再クエリなし (`change-nth` のみ) | ディレクトリ列のみ | 同上 |
| `grep` | `search "$q"` (group 化なし、行単位) | `--disabled` のまま (FTS 側で絞り済み) | `preview {1} --query {q}` でハイライト連動 |

---

## Task 1: モード切替実装

**Files:** `bin/ai-conversation-search`

- [ ] **Step 1: モード別 reload/preview 変数を定義。** `ACS_RELOAD_SCRIPT` (fuzzy 用、現状) に加え `ACS_RELOAD_GREP` (行単位) を追加。preview は `ACS_PREVIEW_BASE="preview {1} --messages 12 --content-chars 150"` + grep 時のみ `--query {q}` 付与。
- [ ] **Step 2: fzf バインド追加。** 例:
  ```
  --bind 'ctrl-f:change-nth(3..)+change-prompt(fuzzy> )+reload($ACS_RELOAD_SCRIPT)...'
  --bind 'ctrl-o:change-nth(4)+change-prompt(dir> )...'
  --bind 'ctrl-g:reload($ACS_RELOAD_GREP)+change-prompt(grep> )...'
  ```
  実際の列番号は現行 TSV (`SESSION_ID \t RESUME_CMD \t DISPLAY`) に合わせ、`--with-nth/--nth` と整合させる。DISPLAY 内の dir 列位置を固定化する (現状は `project basename` が埋め込みのため、dir モード用に列を明示するなら reload の jq 側も修正)。
- [ ] **Step 3: キー解決。** `--bind-fuzzy/--bind-dir/--bind-grep` (CLI) > `ACS_PICK_BIND_*` (env) > defaults。重複・予約語チェック + エラー文。`--help` に追記 (3モード、キー、fzf >= 0.58.0 要件)。
- [ ] **Step 4: 後方互換確認。** 引数なし `pick` の既定動作 (fuzzy、既存フィルタ引き継ぎ) が変わらない。`--days/--repo/--here/--source` が全モードで効く。

## Task 2: 確認

**Files:** `tests/test_pick.sh`、`README.md`

- [ ] **Step 1: 既存 `tests/test_pick.sh` が緑** (CLAUDE.md 手順、実機)。
- [ ] **Step 2: 手動確認 (CI 外のため記録)。** 3モード切替、ヘッダ表示、grep モードでの preview ハイライト連動、`--here` との併用。
- [ ] **Step 3: README/`--help` に3モード記載。** CHANGELOG Unreleased に追加として記載。

---

## Verification

```bash
ACS_TEST_BINARY="$PWD/target/release/ai-conversation-search" sh tests/test_pick.sh
ai-conversation-search pick --help  # 3モードの記載
ai-conversation-search pick  # fuzzy → Ctrl-O → Ctrl-G の切替が動くこと (fzf >= 0.58.0)
```

期待値: 既定は従来通り、dir でディレクトリ絞り込み、grep で本文行ヒット + preview ハイライト。
