# Skill Discovery Scenarios

ユーザーの実発話パターン集。Claude Code が `conversation-search` スキルを正しく選択するか、
リリース前に subagent で検証するためのシナリオ。

各シナリオには:
- **Prompt**: subagent への入力（ユーザーのメッセージそのまま）
- **Expected skill**: 期待される選択（`conversation-search`）
- **Anti-pattern**: 選んではいけない動作（実セッションを読まずに済ませてしまうパターン）
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
- `conversation-search` を呼ばずに、別の検索手段で要約だけ返す
- `grep` / `find` で `.jsonl` を手動探索する
- ToolSearch を最初に走らせる

**Why this scenario:** 実際に再発した事故 (session 29c1a47b-4580-4e4d-b1d3-735d97f99ddc)。
「どのセッション」が日本語トリガーとして弱く、`conversation-search` が選ばれなかった。
PR 番号は raw transcript に原文で残るので FTS で即ヒットするはず。

---

## Scenario 2: 単純な topic-based 過去セッション照会

**Prompt:**
```
authentication の話したセッションってどれだっけ？
```

**Expected skill:** `conversation-search`

**Anti-patterns:**
- 実セッションを読まずに要約だけ返す（session ID が得られない）
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
- 要約を取り出すだけで終わる（resume コマンドを提示しない）

**Why this scenario:** "続きやりたい" / "resume したい" は新規追加トリガー。
意図が明確に "resume" なので確実にこのスキルが選ばれるべき。

---

## Scenario 4: 場所を問う疑問 (どこで)

**Prompt:**
```
あのとき決めたdb設計、どこで話したっけ？
```

**Expected skill:** `conversation-search`

**Anti-patterns:**
- 「db設計について教えてください」と現在の知識で答え始める
- 内容を要約するだけで session を返さない

**Why this scenario:** 「どこで話した」=「どのセッションで話した」と等価だが、語形が違うので
検出されにくい。新規追加トリガー「どこで話した/やった/確認した」の有効性確認。

---

## Scenario 5: 内容把握目的で「過去のセッションを確認」

**Prompt:**
```
どんな内容だったかとかは、過去のセッションを確認して把握して
```

**Expected skill:** `conversation-search`

**Anti-patterns:**
- 実セッションを確認せず、別の手段で要約だけ返す
- 「過去の知識」で内容を要約し始め、実セッションを読まない

**Why this scenario:** 実際に再発した事故 (session 06bc9326-3dfa-4065-b31a-b4c1054f0f5e、2026-06-01)。
主要動詞が「把握して」=内容理解だったため、resume/特定の明示が無く、旧 description の判断テーブルが
"内容把握だけなら要約で十分" と自ら手放していて選ばれなかった。「セッション」という語が明示されたら
resume 目的でなくてもこのスキルを最優先する、という境界に修正済み。
要約より生トランスクリプトの方が忠実、という理由づけが核。

---

## 補足: 検証環境について

このスキルは、他に過去ログを参照しうる手段（要約系の MCP server 等）が同居している環境で
こそ選択精度の意味が出る。検証時はそうした環境を再現した状態で subagent を走らせ、
`conversation-search` が選ばれること・実トランスクリプトを読むことを確認する。
