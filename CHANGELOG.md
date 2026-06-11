# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.12.4] - 2026-06-11

### Fixed

- 日本語など multibyte コンテンツの検索スニペット抽出で char boundary panic が発生する問題を修正（`extract_snippet`/`find_term` を Unicode case-insensitive かつ char-boundary 安全な実装に書き直し）
- `tree <session-id>` が孤児 conversation 行（v0.12.1 以前のバグ等で conversations 行はあるが messages が未登録）に対して空のツリーを返していた問題: raw JSONL トランスクリプトからツリーを構築するフォールバックを追加。malformed 行のスキップ数や記録メッセージ数との差分も warning で明示し、ファイル破損とセッション不一致を区別したエラーを返す
- raw フォールバックと `repair_orphan_conversations` を claude_code source に限定（Codex/OpenCode の rollout ファイルを Claude Code スキーマで誤パースしたり、修復不能な行を削除したりしない）

### Added

- `status` に orphan conversation rows のカウントを追加（1件以上なら `index --all` での修復を stderr で案内）
- `tree` の出力（JSON 含む）に `warning` フィールドを追加（raw フォールバック発動時の通知用）

### Changed

- README / SKILL.md: 手動インストールしたバイナリ（`~/.local/bin` / `~/.cargo/bin`）がプラグインラッパーを shadow して stale になる version drift の注意書きを追加
- CI: release workflow の actions を Node 24 メジャー（checkout@v6, upload-artifact@v7, download-artifact@v8）へ bump

## [0.12.3] - 2026-06-01

### Changed

- `conversation-search` スキルの frontmatter description を、スキル自身が何をするかだけで語る自己記述に書き直し（「過去のセッションを確認/把握して」「どんな内容/話だった(っけ)」「中身を把握」など内容把握目的の日本語トリガーを追記）。「セッション」という語が明示されたら resume 目的でなくてもこのスキルを使う、という判断基準を明確化
- SKILL.md 本文の "When to Use" セクションを、生トランスクリプトを読むことの価値を中心とした記述に整理（判断テーブルを ✅ のみに簡素化）
- `.claude-plugin/plugin.json` / `.claude-plugin/marketplace.json` の description を同方針で更新
- `tests/skill-discovery/` のシナリオを、`conversation-search` が選択されるか・実トランスクリプトを読むかという観点に整理し、Scenario 5（内容把握目的で「過去のセッションを確認」）を追加

## [0.12.2] - 2026-05-28

### Changed

- `conversation-search` スキルの frontmatter description を強化（「どのセッション」「セッションID」「どこで話した/やった/確認した」「続きやりたい」など実発話パターンの日本語トリガーと GitHub PR/issue URL トリガーを追記。1024 chars 上限内に収まるよう調整）
- SKILL.md 本文に "When to Use This vs Memory/Observation Tools" セクションを追加し、resumable session ID が必要なケースと要約で十分なケースの使い分けを明示
- SKILL.md の Examples に "Example 5: GitHub PR/Issue URL" を追加（PR/Issue 番号を `.jsonl` FTS に直接ぶつけるパターン）
- `.claude-plugin/plugin.json` / `.claude-plugin/marketplace.json` の description を OpenCode/Codex 対応の文言に統一

### Added

- `tests/skill-discovery/` を新設し、skill discoverability を subagent ベースで検証する手動テストハーネスを整備（4 シナリオ + RED-GREEN-REFACTOR 手順）
- `CLAUDE.md` の Release セクションにリリース前 skill discoverability 手動チェック項目を追加

## [0.12.1] - 2026-05-25

### Fixed

- 会話インデックスのアトミック性を修正: 単一トランザクション化により `conversations.message_count > 0` なのに `messages` テーブルにレコードがない孤立行が発生するバグを解消
- Claude Code の resume セッションが同一 `message_uuid` を再エミットした際の PRIMARY KEY 衝突を `INSERT OR IGNORE` で安全に処理（衝突によりトランザクション全体がロールバックする問題を回避）
- `index` 実行時に既存 DB の孤立 conversation 行を自動修復（`repair_orphan_conversations`）。次回 index で JSONL から再構築される
- ファイル mtime 取得失敗時に永久スキップせず次回再試行するよう変更（理由をログ出力）
- `message_count` をパース時の JSONL 行数ではなく実 INSERT 後の `COUNT(*)` から再計算

## [0.12.0] - 2026-04-23

### Added

- `pick` コマンドをライブ全文検索対応に刷新（検索しながら結果を絞り込んでセッション選択）
- セッション単位でのグループ化検索機能を実装

## [0.11.0] - 2026-04-11

### Added

- `--here` オプションを追加して現在のディレクトリでの会話検索をサポート
- 対話型セッションピッカー機能を追加（検索結果からセッションを選択して再開）
- 会話検索スキルの使用ケースを拡張

## [0.10.0] - 2026-04-08

### Added

- 複数の Claude プロファイルディレクトリを自動検出してスキャン（`~/.claude`, `~/.claude-personal` 等）
- `CONVERSATION_SEARCH_EXTRA_DIRS` 環境変数で追加スキャンディレクトリを指定可能（コロン区切り、`~` 展開対応）
- ディレクトリ検出・読み取り失敗時の警告ログ出力

### Fixed

- summarizer プロジェクトハッシュのキャッシュが複数ディレクトリ間で誤って共有されるバグを修正

## [0.9.0] - 2026-03-30

### Added

- `status` コマンドを追加（索引の健全性・カバレッジ表示）
- 検索結果をセッション単位でグループ化する `--group-by-session` オプション
- 完全一致検索用の `--exact` フラグ（FTS5 演算子インジェクション対策含む）
- 検索統計（スキャン対象セッション数、マッチ数など）の追跡・表示
- インデックスされていないファイルの警告を検索結果に表示
- JSON 出力に `resume_command` フィールドを自動注入

### Fixed

- FTS integrity-check が read-only 接続で常に失敗するバグを修正

## [0.8.0] - 2026-03-28

### Changed

- 自動インデックスを背景プロセスに移行（TTLベースのクールダウン付き、CLIをブロックしない設計に）
- スキルのインストール処理を関数化し、バージョン指定・自動アップグレードに対応

### Removed

- `--no-index` / `--force-index` オプションを `search`, `list`, `context` コマンドから削除（バックグラウンドインデックスに置き換え）

## [0.7.2] and earlier

See [git log](https://github.com/yoshi47/ai-conversation-search/commits/main) for previous changes.
