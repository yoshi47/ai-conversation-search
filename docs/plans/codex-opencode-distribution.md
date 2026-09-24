# Codex / OpenCode への配布経路を用意する

## Context

conversation-search スキルは Claude Code プラグイン（`.claude-plugin/`）としてだけ配布している。
Codex と OpenCode 向けの導入手順は README に無く、手元では chezmoi でリポジトリの clone を直接指していた
（`~/.codex/skills/conversation-search` への symlink、`opencode.jsonc` の `"skills"` にローカルパス）。
これを、このリポジトリ自身が提供する配布経路に置き換える。

## 確認済みの事実

- Codex 0.154.0 は Claude 形式の `.claude-plugin/marketplace.json` をそのまま読める。
  一時 `CODEX_HOME` で `codex plugin marketplace add <repo>` → `codex plugin add conversation-search@ai-conversation-search`
  が成功し、`codex exec` のスキル一覧に `conversation-search` が出た。リポジトリ側の変更は不要
- OpenCode v2 は `"skills"` に HTTP(S) URL を書ける。`<base>/index.json` のカタログを読み、
  `<base>/<name>/<file>` を取得する。`version` を上げるとキャッシュが更新される（https://opencode.ai/v2/docs/skills/）。
  ローカルの `python3 -m http.server` でカタログを配信し、`opencode run` のスキル一覧に出ることを確認した
- OpenCode にはプラグイン経由でスキルを配る仕組みの記載が無い（https://opencode.ai/v2/docs/plugins/）
- Claude Code プラグインは `bin/` を PATH に載せるが、Codex / OpenCode では CLI を別途入れる必要がある
  （README の Manual Installation の wrapper を `~/.local/bin` に置く手順）

## 変更内容（リポジトリ）

1. `skills/index.json` を追加（OpenCode HTTP カタログ）
   ```json
   {
     "skills": [
       { "name": "conversation-search", "version": "0.17.0", "files": ["SKILL.md", "REFERENCE.md"] }
     ]
   }
   ```
2. `scripts/bump-version.sh` で `skills/index.json` の `version` も更新する
   （既存の `sedi "s/\"version\": \".*\"/.../"` と同じ置換。完了時の echo 一覧と Verification の grep 対象にも追加）
   - プロジェクト `CLAUDE.md` の「Bumping Version」の更新対象一覧にも `skills/index.json` を足す
   - 同じく `CLAUDE.md` に運用ルールを1行: OpenCode は `version` が変わるまでキャッシュを使うため、
     SKILL.md / REFERENCE.md の変更はバージョンを上げたときに OpenCode へ届く
3. `README.md` に Codex / OpenCode の導入節を追加
   - Codex:
     ```bash
     codex plugin marketplace add yoshi47/ai-conversation-search
     codex plugin add conversation-search@ai-conversation-search
     ```
   - OpenCode（`opencode.json(c)`）:
     ```jsonc
     { "skills": ["https://raw.githubusercontent.com/yoshi47/ai-conversation-search/main/skills/"] }
     ```
   - どちらも CLI は Manual Installation の wrapper で入れる旨を書く
4. `skills/conversation-search/SKILL.md` の Prerequisites（「command not found なら plugin を入れ直す」）に、
   Codex / OpenCode では wrapper を `~/.local/bin` に入れる旨を1行足す
5. `CHANGELOG.md` の `[Unreleased]` に Added として記載

リリース（タグ）はしない。OpenCode は main の raw URL を読み、Codex は marketplace の git を読むので、マージ時点で有効になる。

## 変更内容（手元、chezmoi）

作業場所は worktree `~/ghq/github.com/yoshi47/chezmoi-dotfiles.worktrees/opencode-v2-migration`（ブランチ `chore/opencode-v2-migration`、PR #25）。
前回の未コミット変更はここに移されている。

- 前回の未コミット変更を戻す: `.chezmoiignore` の `.codex/skills` 例外 3 行、`dot_codex/skills/` を削除。ホームの `~/.codex/skills/conversation-search` symlink も削除
- `dot_config/opencode/opencode.jsonc` の `"skills"` をローカルパスから上記 raw URL に差し替え
- `dot_codex/modify_private_config.toml.tmpl` の MANAGED_CONFIG に、inkmark と同じ形で追加
  ```toml
  [plugins."conversation-search@ai-conversation-search"]
  enabled = true
  ```
  marketplace の登録（`[marketplaces.*]`）は modify スクリプトが保持するだけで管理していない（inkmark と同じ扱い）。
  そのため `codex plugin marketplace add yoshi47/ai-conversation-search` と `codex plugin add ...` はホームで一度だけ手で実行する
- ホームへの反映は該当ファイルだけ: `chezmoi --source <worktree> apply ~/.config/opencode/opencode.jsonc ~/.codex/config.toml`。
  main checkout での全体 apply はしない（opencode 設定が v1 に戻るため）。
  先に `--dry-run --verbose` で差分を見てから適用する

## 検証

- `codex exec` と `opencode run` のスキル一覧に `conversation-search` が1つだけ出る（重複しない）
- `tests/skill-discovery/scenarios.md` のシナリオを1つ、Codex と OpenCode それぞれで実行し、スキルが選ばれて CLI が動くこと
- `bash scripts/bump-version.sh` を実行せず、sed の置換が `skills/index.json` に当たることを dry-run（`sed -n`）で確認

## スコープ外

（実装後に更新）Codex の hooks はスコープに入れた。公式ドキュメントに、Codex が `CLAUDE_PLUGIN_ROOT` をセットし
`hooks/hooks.json` を自動で読むと書かれている。一時 CODEX_HOME で hook を trust して試すと、UserPromptSubmit と Stop が完走し、
`~/.conversation-search/.last-auto-index` が更新された。README は「承認すると自動インデックスが効く」に直し、
UUID リマインダの文面は Claude 固有の語（Skill tool、`~/.claude/projects` だけ）を外してツール中立にした
- OpenCode 用の JS プラグイン（hook 相当）
- OpenCode がオフラインで起動したときのカタログ取得失敗の挙動（未確認）。raw.githubusercontent の CDN キャッシュで反映が数分遅れることはある
