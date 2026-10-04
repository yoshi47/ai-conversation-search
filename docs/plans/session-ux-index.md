# セッション UX 改善 — 目次 (詳細は機能別プランへ)

index/FTS の強みは維持し、「探して戻る」までの導線を短くする。
実装プランは機能ごとに分割した。こちらは目次のみ。

| # | プラン | ファイル |
|---|---|---|
| ① | `resume-spec` — 非起動ハンドオフ | `docs/plans/resume-spec.md` |
| ② | `last` — 最新セッションに直帰 | `docs/plans/last.md` |
| ③ | `preview` — 単体プレビュー + ハイライト | `docs/plans/preview.md` |
| ④ | `pick` 3モード切替 — fuzzy / dir / grep | `docs/plans/pick-modes.md` |
| ⑤ | `list` 強化 — project_exists/project_basename + exclude-project/exclude-repo | `docs/plans/list-enrich.md` |

依存関係: ②は①の JSON を使う、③は①の session 解決を使う、④は③の `preview` を呼ぶ、⑤の `matches_exclude` は②と共有。

非ゴール (別スペック候補): `export`/`gc`/`top`、config ファイル、バックエンド追加、MCP サーバ化。
