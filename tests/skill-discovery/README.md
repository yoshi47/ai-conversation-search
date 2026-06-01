# Skill Discovery Tests

`conversation-search` スキルがユーザーの実発話に対して正しく選択されるかを
subagent ベースで検証する手動テストハーネス。

## 目的

writing-skills ベストプラクティスの **RED-GREEN-REFACTOR** に倣う:

- **RED:** skill が無い / description が弱い状態で subagent に投げて、
  `conversation-search` が選ばれない（実セッションを読まずに済ます）ことを確認 (baseline)
- **GREEN:** description 改善後に同じ prompt を投げて `conversation-search` が
  選択されることを確認
- **REFACTOR:** 新しい言い回しで選ばれなかった場合、シナリオを追加して description を
  強化する

## 対象シナリオ

`scenarios.md` 参照。各シナリオに prompt / expected skill / anti-pattern が
記載されている。

## 実行方法 (手動・リリース前のみ)

CI には組み込まない。subagent 起動コストが高く、また他に過去ログ参照手段が同居する
環境を CI で再現するのが困難なため。

### 前提

1. 選択精度を見るため、他に過去ログを参照しうる手段（要約系の MCP server 等）が
   同居した環境を再現しておくのが望ましい（無くても本スキルが選ばれるかは確認可能）
2. `ai-conversation-search` CLI がローカルで動作すること
   ```bash
   ai-conversation-search --version
   ```

### 手順

各シナリオについて、Claude Code 上で以下を実施:

1. **新しい会話を開く** (`/clear` または別ターミナル)
2. `scenarios.md` の Prompt をそのまま入力
3. 観察項目:
   - 最初に呼び出されたスキル / ツール
   - `Skill` tool で `conversation-search` が呼ばれたか
   - `conversation-search` を呼ばずに済ませていたら **REGRESSION**

### 結果記録

シナリオごとに以下を残す (PR description に貼り付け推奨):

```
- Scenario 1 (PR URL): ✅ conversation-search / ❌ 選ばれず
- Scenario 2 (authentication): ✅ / ❌
- Scenario 3 (続きやりたい): ✅ / ❌
- Scenario 4 (どこで話した): ✅ / ❌
```

### 失敗時の対応

- どのシナリオで選ばれなかったか特定
- どの語が description にあれば選ばれたかを推定
- `skills/conversation-search/SKILL.md` の frontmatter description を更新
- 同じシナリオで再度検証 (GREEN になるまで)
- 新しい取りこぼしパターンが見つかったら `scenarios.md` に追加 (REFACTOR)

## リリースチェックリストへの組み込み

`CLAUDE.md` の Release セクションに以下を追加することを推奨:

```
リリース前手動チェック:
- [ ] tests/skill-discovery/scenarios.md のシナリオを最低 1 件 subagent で実行し、
      conversation-search が選択されることを確認
```
