# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
