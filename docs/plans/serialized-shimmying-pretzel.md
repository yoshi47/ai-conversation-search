# SKILL.md 修正プラン — 監査で確定した誤用フリクションを塞ぐ

## Context

過去に `conversation-search` スキルが読み込まれた 145 セッション（observer / 本セッション除外後、`ai-conversation-search search --exact` の重複排除で抽出）を10エージェントで分担精読し、フリクションを10テーマに集約→敵対的検証にかけた。144セッション中 成功121 / 失敗3 / ツールエラー14 / 誤トリガー6。

致命的障害はなく、`verdict=confirmed` かつ SKILL.md のドキュメント修正だけで塞げる誤用パターンが5件残った。いずれも「LLM がスキルの意図を取り違えて生 jsonl を触る / JSON parse に失敗する / 狭い範囲のまま断定する」類。Rust/バイナリの変更は不要だが、**ユーザーに届けるにはバージョンバンプ＋リリースが必要**（下記参照）。本プランはこの5件を対象とする（#8 タイトルFTS対応は Rust 変更が必要なため別プランに切り出し）。

対象ファイルは1つ: `skills/conversation-search/SKILL.md`（全445行）。

> **デプロイの前提（Risk レビューで判明・要注意）**: スキル本文はプラグインキャッシュから読まれ、`bin/ai-conversation-search:9-14` の自己更新は**バイナリのみ**でスキル markdown には触れない。手元のインストール済みプラグインは `~/.claude/plugins/installed_plugins.json` で **v0.12.4 に固定**されており、以降5リリースぶんの SKILL.md 変更が届いていない。CHANGELOG（`CHANGELOG.md:118-177`）でも過去の SKILL.md のみの変更は**例外なくバージョンバンプ＋リリース経由で配布**されている。よって「doc 変更だからリリース不要」は誤り。本プランは `./scripts/bump-version.sh <patch>` によるパッチバンプ＋タグ push リリースまでを含める（破壊的変更ではないので minor ではなく patch）。

## 修正内容（5件）

### 修正1 — [high] サブエージェント委任・python/jq 直パースの抜け穴を塞ぐ（テーマ#3）

**問題**: `CRITICAL CONSTRAINTS`（`SKILL.md:26`）は `grep/find/cat` を禁じるが、(a) 調査をサブエージェントに丸投げすると制約が継承されず Bash/Grep で生 jsonl を探索し失敗（`eb29e399` は68.5kトークン浪費）、(b) `python3 json.loads` / `jq` での生 jsonl 直パースを「grep ではないから可」と解釈するケースが再発。

**編集**: `SKILL.md:26` に「生jsonlのpython/jq直パースも禁止」を足す（既存 `SKILL.md:30-32` は「このツールの `--json` を jq に流すのは可」を既に述べているので、そこは繰り返さず raw jsonl 側だけを禁ずる）。加えてサブエージェント句を追加。

- `SKILL.md:26` を次に置換（新規情報は「raw .jsonl の python/json.loads/jq 直パース禁止」の一点。`--json` を jq に流す件は下の :30-32 が担当）:
  `- DO NOT use grep, find, cat, or any manual file operations on .jsonl files — this also bans parsing raw .jsonl with \`python3\`/\`json.loads\`/\`jq\` (piping this tool's own \`--json\` is fine; see below)`
- `SKILL.md:29`（`ONLY use ai-conversation-search commands ...`）の直後に新バレットを追加:
  `- When you delegate past-session investigation to a subagent (Task tool), the subagent does NOT inherit these constraints — your delegating prompt MUST tell it to use \`ai-conversation-search\` (give the invocation) and MUST NOT tell it to grep/find/read \`~/.claude/projects\` or \`.jsonl\` files directly`

> **編集1の位置づけ（Risk レビュー指摘）**: このサブエージェント句は「委任側が spawn 時に制約注入を思い出す」ことに依存する prose 対策で、構造的に確実ではない（リポジトリは 2026-07-03 のトリガー不発で「description tuning alone is probabilistic」と結論し hook 化した前例あり: `hooks/session-mention-reminder.sh:8-9`）。決定的な解は `PreToolUse` hook で Bash の `~/.claude/projects/**/*.jsonl` アクセスを弾くことだが、本リポジトリに `PreToolUse` hook は現状なく（`hooks/hooks.json` は Stop / UserPromptSubmit のみ）新規インフラになるため**本プランのスコープ外**。編集1は安価な暫定策と明記して採用する。

### 修正2 — [high] `--json` の parse 失敗（stderr 混入・スキーマ当て推量）（テーマ#4）

**問題**: 4件中2件（`d37c7e3a`, `6028f6a8`）で `search --json 2>&1 | jq` が CLI 診断メッセージ（`Note: showing first N results...`）を JSON に混入させ `Extra data` で落ちた。加えて `context_snippet` フィールドを `text`/`snippet`/`.messages[]` と誤想定して本文が常に空になる。現行ドキュメントは実際の JSON 出力例（フィールド名込み）を一切持たない（確認済み: `context_snippet`/`session_id`/`resume_command` の文字列は全445行に登場しない）。CLI 側は `src/cli.rs:1054-1063` で stderr 分離済みのためコード変更不要。

**編集**: `## Command Reference`（`SKILL.md:223`）直後、`### Search`（`SKILL.md:225`）見出しの前に、以下の**本文のみ**を挿入する（Risk レビュー指摘で JSON ブロックは廃し、フィールドは1行リストに圧縮）。挿入する実テキストは次の見出し行＋2バレット:

```
### Reading `--json` output (two parse traps)

- Pipe as `... --json 2>/dev/null | jq`, never `2>&1`. The CLI writes diagnostics (`Note: showing first N results...`, scan counts) to stderr on purpose; folding them into stdout makes `jq`/`json.loads` fail with "Extra data".
- Use the real field names — don't guess `text`/`snippet`/`.messages[]`. A `search`/`list` row has: `context_snippet` (matched text; `full_content` with `--content`), `session_id`, `resume_command`, `source`, `message_type`, `project_path`, `timestamp`. Size output with `--limit`/`--content-chars`, never by truncating the stream (`head -c` before `json.load` corrupts it).
```

（`### Reading ...` の見出しと2バレットのみを SKILL.md に書く。上の ` ``` ` フェンスは plan の提示枠であって挿入対象ではない。）

> 注: フィールド名は Risk レビューで v0.16.1 の実出力と一致を確認済み（`ai-conversation-search search "test" --json --limit 1` の返却キー）。実装時にもう一度 `search --json | jq '.results[0]'` で最終確認する。

### 修正3 — [high] 狭い範囲のまま自信満々に誤ったセッションを提示（テーマ#5）

**問題**: デフォルト範囲で検索した結果を「網羅した」かのように断定するパターンが4件。`af4e728a` は誤った `--repo` のまま無関係な今日のセッションを「該当」と報告、`a8d3a19a` は `--days` 絞り込みを開示せず断定しユーザーに再検索を強制された。根本原因は「候補の本文とクエリ条件の一致確認不足」。

**編集**: `### Level 4: Present Results` の冒頭（`SKILL.md:191` `**Format results for the user:**` の直前）に確認ゲートを追加:

```markdown
**Before asserting a match:**
- Confirm the candidate's `context_snippet`/content actually matches the query's concrete
  anchors (proper nouns, dates, PR/issue numbers, the described context). If the tie is weak,
  present it as "最有力候補" with a caveat, not as a confirmed hit.
- State the search scope you used (`--days`/`--repo`/`--date`, or "全期間 (no filter)"). A
  narrow scope silently reported as exhaustive is how wrong sessions get presented as answers.
```

### 修正4 — [medium] TodoWrite 不在コンテキストで「MANDATORY」が実行不能・L27 と矛盾（テーマ#6）

**問題**: サブタスク/バックグラウンド等で TodoWrite が使えないコンテキストが8セッションで発生。`SKILL.md:13` の `you MUST use the TodoWrite tool` と `SKILL.md:27` `DO NOT skip the todo creation step` が実行不能になり、毎回モデルが同じ言い訳を生成。

**編集**:
- `SKILL.md:13` の文末に条件を追記:
  `**Before doing ANYTHING else, you MUST use the TodoWrite tool to create this exact checklist:** (If TodoWrite is not in this session's tool list — e.g. a subagent/background context — skip the checklist and go straight to Level 0/1; do not block on it.)`
- `SKILL.md:27` を次に置換して矛盾を解消:
  `- DO NOT skip the todo creation step when TodoWrite is available`

### 修正5 — [medium] フラグの罠: `--no-tools` が本文を消す / 短縮IDで resume 失敗（テーマ#9）

**問題**: `4e78b5ab` で `--no-tools` がブラウザ取得結果に含まれる本文（店名等）を丸ごと落とし4エージェント再実行。`3b324e76` で `tree` 用の8文字プレフィックスをそのまま `claude --resume` に渡し `no session match`。

**編集**:
- `SKILL.md:313`（`--no-tools` の行）の Effect を追記:
  `| \`--no-tools\` | Drop \`[Tool: X]\` / \`[Tool result]\` / interrupt nodes, and empty bodies. **Also drops text that lives inside tool results** (e.g. pasted browser-fetch output) — skip this flag, or narrow with \`--role\`, right after a fetch/tool-heavy turn |`
- `SKILL.md:306`（プレフィックス曖昧性の説明段落の末尾、次の空行の前）に一文を追加（`:304` は文中なので不可）:
  `The short prefix works for \`tree\`/\`context\` only. \`claude --resume\` does NOT resolve prefixes — always put the full \`session_id\` (UUID) from the \`tree\`/search result into the resume command.`
- `### List`（`SKILL.md:275`）節の末尾、`### Status`（`SKILL.md:288`）見出しの直前に1行の相互参照を追加（`.truncated` 本文の重複は避け、誘導のみ）:
  `Like \`search\`, \`list\` is capped by \`--limit\` — the default cap can return only sessions newer than a target date and drop the older one you want. See the \`.truncated\` note below.`

## 対象外（検証で棄却したため本プランに含めない）

- テーマ#1 claude-mem 競合（overstated / hook で対応済み・3ヶ月再発なし）
- テーマ#2 stale/shadow バイナリ（already-addressed: `bin/ai-conversation-search:94-129` + `SKILL.md:80-93`）
- テーマ#7 observer ノイズ（already-addressed: v0.14 BM25 + v0.15 prune-observer）
- テーマ#8 タイトルFTS未対応（confirmed だが Rust 変更＋リリースが必要 → 別プラン）
- テーマ#10 Codex パニック（already-addressed: `src/indexer/codex.rs:475,556`）

## Verification

ドキュメント変更のため自動テストは無い。以下で確認する:

1. **整合性チェック**: 編集後の `SKILL.md` を通読し、(a) 修正4で `MUST`（L13）と `DO NOT skip`（L27）が矛盾しないこと、(b) 修正5の `.truncated` 相互参照が既存の `SKILL.md:338-341` を重複させていないこと、を目視確認。
2. **JSON 例の正確性**: `ai-conversation-search search "<任意語>" --json | jq '.results[0]'` を1回実行し、修正2で貼ったキー名（`context_snippet` 等）が実出力と一致することを確認。差異があれば例を実出力に合わせる。
3. **フリクション再現の解消（机上）**: 監査の代表 sid（`eb29e399`=委任, `d37c7e3a`=stderr混入, `af4e728a`=誤repo断定）の失敗手順を、修正後の該当バレットが明示的に禁止/回避しているか照合する。
4. **plan-document-reviewer + 観点別レビュー**をプラン提示前に実行済みであること（CLAUDE.md ワークフロー）。

## Rollout（マージ後 = 本プランに含む）

SKILL.md 変更をユーザーに届けるには、このリポジトリの確立フロー（CHANGELOG が示す通り doc-only 変更も毎回バンプ＋リリース）に従う。

1. `CHANGELOG.md` に本変更の節を追記（リリースノートは CHANGELOG の該当節から自動生成される: 直近コミット `aab5fbb` 参照）。
2. `./scripts/bump-version.sh <patch>`（例 0.16.1 → 0.16.2）を実行。`Cargo.toml` / `Cargo.lock` / `.claude-plugin/plugin.json` / `.claude-plugin/marketplace.json` / `bin/ai-conversation-search` の `ACS_WRAPPER_VERSION` が一括更新される。
3. コミット → `git tag v<version>` → `git push origin v<version>`（pre-push hook が重複タグを弾く）。GitHub Actions がバイナリをビルド＆リリース。
4. ユーザー側はプラグイン更新で新スキルを取得（自動更新はバイナリのみなので、スキル反映にはプラグイン/marketplace の更新が要る点を周知）。

> Rust コードは無変更だが、スキル配布のためにバンプが要る。破壊的変更ではないので patch。
