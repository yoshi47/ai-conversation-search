# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
