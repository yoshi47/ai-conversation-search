# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- `search --sort <relevance|recent>`: 並び順の選択。デフォルトは `relevance`（bm25）。`recent` で従来の新着順に戻せる

### Changed

- **検索結果の並び順が bm25 関連度順になった**（従来は新着順）。実 DB（780,740 メッセージ）での計測では、単語 1 つの検索で上位 12 件のうち 10 件を占めていた claude-mem observer の巨大メッセージ（6,783〜51,862 字）が、bm25 の文書長正規化によって上位から消え、12 件すべてが実会話になった
- **多語クエリが AND から OR になった**（FTS 経路のみ）。従来はスペース区切りの語を FTS5 の AND で連結していたため、`パッケージ アップグレード ドキュメント` のような組み合わせは 0 件になりやすかった。OR + bm25 により、より多くの語に一致する文書が上位に来る。`--exact` と明示的な `AND`/`OR`/`NOT` を含むクエリは従来どおり
- `--group-by-session` の代表メッセージが「最新の一致」から「最良スコアの一致」に変更。`--sort=recent` では従来どおり最新（LIKE フォールバック経路では `--sort` によらず従来どおり最新）
- **3 文字未満の語を含むクエリもランキングされるようになった**。従来は短語が 1 つでもあるとクエリ全体が FTS を迂回して全表 LIKE スキャン（AND + 新着順）に落ちていた。現在は 3 文字以上の語を FTS + bm25 で照合し、短語は結果に対する必須の部分一致条件として適用する。実 DB（780,740 メッセージ）での `デプロイ 失敗`（`失敗` が 2 文字）の計測では、上位 6 件が observer transcript（11,444〜91,464 字）6/6 から実会話（99〜1,307 字）6/6 になった。クエリ単体の実行時間は 7.596s → 0.125s（約 60 倍）、CLI 全体では 11.9s → 4.1s（残りは統計収集などの固定コスト）。候補が多く短語がほとんど当たらない最悪ケースでも 2.3s
- **全語が 3 文字未満のとき**（例: `認証 実装`）のみ従来どおり LIKE フォールバック（AND + 新着順）で、`--sort` も効かない。ランキング信号が一切ないため意図的に据え置き

### Fixed

- `OR` 演算子を含むクエリが FTS に届いていなかった。短語判定が語長だけを見ていたため `OR`（2 文字）自体が短語とみなされ、`Docker OR リファクタ` が `%OR%` を含む LIKE-AND として実行されていた（実 DB で本来 7,264 件のところ 136 件）。`AND` / `NOT` は 3 文字のため影響なし

### Note

- 明示的な `AND`/`OR`/`NOT` 演算子は**全オペランドが 3 文字以上のときだけ** FTS にそのまま渡す。3 文字未満のオペランドは FTS でトークン化されず演算子の意味を黙って書き換えてしまうため（`A AND 失敗` が 0 件に、`A NOT 失敗` が何も除外しなくなる）、その場合は演算子を literal として扱う LIKE 経路に回す。短いオペランドを FTS に届けたい場合は引用符で囲む

## [0.13.1] - 2026-07-03

### Added

- UserPromptSubmit hook（`hooks/session-mention-reminder.sh`）: プロンプトに session UUID とセッション系ワードの両方が含まれるとき、`conversation-search` スキル利用のリマインダを additionalContext として注入。description チューニングだけでは確率的だった「UUID 直指定」ケースを決定論的にトリガーする。fail-open 設計（jq 不在・不正 JSON・パターン不一致はすべて silent no-op、ユーザーのプロンプトを絶対にブロックしない）
- `tests/skill-discovery/`: Scenario 6（session UUID 直指定＋結論の検証依頼）を追加。「主タスクがコード検証で UUID が既知だと『検索不要』と合理化されてスキルが選ばれない」再発事故（2026-07-03）を記録

### Fixed

- Manual Install をラッパースクリプト配布に変更し、手動インストールで `pick` / `setup-hooks` が使えなくなる問題を修正

### Changed

- SKILL.md description: 「session UUID がプロンプトにあれば、transcript の場所が自明でも・セッションが別タスク（結論検証・続き作業）の材料でも必ずこのスキルを使う。`~/.claude/projects` を find/grep しない」という境界を追記。decision table にも該当行を追加
- SKILL.md: `context` コマンドの引数表記を `<MESSAGE_UUID>` に明確化（session UUID を渡す誤誘導を防止）
- CI: clippy + rustfmt lint、CI workflow、lefthook hooks を追加し、rustfmt を全体適用

## [0.13.0] - 2026-06-17

### Added

- `hook` subcommand: lightweight trigger for background indexing, designed for use as a Claude Code Stop hook (exits in <50ms when index is fresh)
- `setup-hooks` subcommand: idempotently adds a Stop hook to `~/.claude/settings.json` so the search index updates automatically after every conversation turn
- Fast path in wrapper script: `hook` subcommand skips binary download and DB init checks

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
