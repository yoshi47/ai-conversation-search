# Skill Discovery Scenarios

ユーザーの実発話パターン集。Claude Code が `conversation-search` スキルを正しく選択するか、
リリース前に subagent で検証するためのシナリオ。

各シナリオには:
- **Prompt**: subagent への入力（ユーザーのメッセージそのまま）
- **Expected skill**: 期待される選択（`conversation-search`）
- **Anti-pattern**: 選んではいけないツール／動作
- **Why this scenario**: なぜこのパターンが落とし穴になりやすいか

---

## Scenario 1: GitHub PR URL + 「どのセッション」

**Prompt:**
```
このPRのレビュー・設計確認してたのってどのセッションですか？
https://github.com/org/repo/pull/23064
```

**Expected skill:** `conversation-search`

**Anti-patterns:**
- `mcp__plugin_claude-mem_mcp-search__search` などの memory/observation MCP ツールを呼ぶ
- `grep` / `find` で `.jsonl` を手動探索する
- ToolSearch を最初に走らせる

**Why this scenario:** これは実際に再発した事故 (session 29c1a47b-4580-4e4d-b1d3-735d97f99ddc)。
「どのセッション」が日本語トリガーとして弱く、競合 MCP に取られた。PR 番号は raw transcript に
原文で残るので FTS で即ヒットするはず。

---

## Scenario 2: 単純な topic-based 過去セッション照会

**Prompt:**
```
authentication の話したセッションってどれだっけ？
```

**Expected skill:** `conversation-search`

**Anti-patterns:**
- memory/observation 系ツールでの代替（要約が返ってきても session ID は得られない）
- ユーザーに「どのプロジェクトですか？」と聞き返す（まず全件検索すべき）

**Why this scenario:** "〜だっけ？" 系は既存 description にあるが、「セッションってどれ」という疑問詞構造の
確認用。topic query の典型例。

---

## Scenario 3: Resume 意図の明示

**Prompt:**
```
昨日のセッションでやった話の続きやりたい
```

**Expected skill:** `conversation-search`

**Anti-patterns:**
- 「どんな話でしたか？」と漠然と聞き返す
- memory ツールで要約を取り出すだけで終わる（resume コマンドを提示しない）

**Why this scenario:** "続きやりたい" / "resume したい" は新規追加トリガー。
意図が明確に "resume" なので memory 系ではなくこちらが選ばれるべき。

---

## Scenario 4: 場所を問う疑問 (どこで)

**Prompt:**
```
あのとき決めたdb設計、どこで話したっけ？
```

**Expected skill:** `conversation-search`

**Anti-patterns:**
- 「db設計について教えてください」と現在の知識で答え始める
- memory ツールで内容を要約するだけで session を返さない

**Why this scenario:** 「どこで話した」=「どのセッションで話した」と等価だが、語形が違うので
検出されにくい。新規追加トリガー「どこで話した/やった/確認した」の有効性確認。

---

## 補足: claude-mem が共存する環境での実行

このスキルが claude-mem などの MCP memory/observation server と共存する場合に
特に意味がある。検証時は **両方インストールされた状態**で subagent を走らせること。
片方しかなければ常にもう片方が選ばれるので競合検出にならない。
