# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.17.0] - 2026-09-24

### Changed (breaking)

- **OpenCode の読み取りを v2 のスキーマに置き換えた。v1 の DB は読まない**。v2 では DB（パスは同じ `~/.local/share/opencode/opencode.db`）の `session` / `message` / `part` テーブルが `session_v2` / `session_message` に置き換わった。そのため従来のクエリは失敗し、`Warning: failed to index OpenCode conversations` が出て v2 のセッションが 1 件も入らなかった。v2 に上げると OpenCode 側の DB から v1 のテーブルは消えるので、互換経路は残していない。OpenCode v1 のままだと、インデックス時に同じ警告が出て OpenCode 分がスキップされる（インデックス済みのセッションは残る）

  - 取り込むメッセージは `user` / `assistant` だけ。`system` / `synthetic`（system-reminder）/ `shell` / `compaction` 等は、v1 で user/assistant 以外の role を捨てていたのと同じ扱いにした
  - v2 の `session_v2.time_updated` は、メッセージが追記されても更新されないことがある（手元の DB では大半のセッションで最終メッセージより古かった）。そのため同期カーソルと再インデックスの判定には、セッションとメッセージの `time_updated` の大きいほうを使う

## [0.16.2] - 2026-09-15

セッションのタイトル（会話サマリ）で検索が引けるようになった。あわせて、過去セッションの監査で確定したスキルの誤用フリクションを SKILL.md 側で塞いだ。

### Added

- **タイトル（`conversation_summary`）一致を検索結果に含めるようにした**。タイトルは `conversations` テーブルにあり messages の FTS 索引に入っていないため、本文に同語が無いとタイトルで覚えているセッションが 0 件になっていた。本文 FTS の結果を作ったあと、本文でヒットしていないタイトル一致セッションだけを会話単位のフィルタで引いて合流させる（新テーブルや再インデックスは不要）。ピッカーの live 検索（`search --group-by-session`）経路も同対応で、fzf ヘッダの「titles + content」が実態と一致するようになった

  - 本文と重複するセッションは除外するため、両方に一致する場合は本物の本文行が代表・上位に残る（代表のハイジャックを防ぐ）
  - 日付 / リポジトリ / ソースのフィルタは `conversations` の列（`first_message_at`/`last_message_at` の重なり）で判定するため、代表メッセージの時刻に依存しない
  - 引用句や FTS 演算子クエリ（`foo NOT bar` 等）ではタイトル照合をスキップする。演算子を無視して AND すると `NOT` が反転し除外語をタイトルで拾ってしまうため（本文 FTS 側は従来どおり演算子を尊重する）

  注記: `--group-by-session` の `match_count` は、タイトルのみ一致したセッションを 1 と数える（本文に検索語を含むメッセージがあるわけではない）。統計合計 `total_matched_messages` はこれらを含めない

### Fixed

- **`conversation-search` スキルの SKILL.md で、監査で確定した誤用フリクション5件を塞いだ**（ドキュメントのみ、Rust 無変更）。生 `.jsonl` を `python3`/`json.loads`/`jq` で直接パースする抜け穴とサブエージェント委任時の制約非継承、`--json` の stderr 混入とスキーマ当て推量（`search` と `list` でフィールドが違う）、狭い検索範囲のまま断定する挙動、TodoWrite が使えないコンテキストで MANDATORY 手順が実行不能になる矛盾、`--no-tools` が本文を落とす条件と短縮 ID を `claude --resume` に渡すと失敗する点、をそれぞれ明文化した

## [0.16.1] - 2026-08-20

0.16.0 のリリースノートと `REFERENCE.md` に載せた遡及手順が誤っていたため、その訂正のみ。コードに変更はない。

### Fixed

- **「`index --all --force` で `[Task notification]` を既存インデックスに遡及できる」という記述を訂正した**。実際には遡及しない。`--force` が外すのは `claude_code_sync_state` の mtime によるファイル単位のスキップだけで、その先の `do_index_conversation` には「インデックス済みセッションでは未登録 UUID のメッセージしか INSERT しない」という second-level のスキップがある。本文の導出ロジックを変えても既存行は書き換わらない。正しい遡及手順（SQL の直接適用）を `REFERENCE.md` と 0.16.0 の Migration 節に載せた

  ドキュメントのみの変更だが patch を上げているのは、`REFERENCE.md` がプラグインの同梱物で、`claude plugin update` がバージョン番号でしか更新を判定しないため。番号を上げないと訂正が誰にも届かない

## [0.16.0] - 2026-08-20

セッション UUID を直接渡されたときの摩擦への対応。直前に終わったセッションが引けない点と、`tree` に絞り込みが一切なく利用側が jq を書く羽目になっていた点が主。

### Added

- `tree --role user|assistant`: 片側の発言だけに絞る
- `tree --no-tools`: `[Tool: X]` / `[Tool result]` / 中断ノード、および本文が空になったノードを落とす
- `tree --flat`: 入れ子をやめて平坦な一覧を返す
- `tree --content` / `--content-chars N`: 本文を出す（既定 300 字）。`search` と同じ契約
- `tree` の JSON に `returned_messages` が付いた。`total_messages` は「セッション内の件数」の意味を保つ。絞り込みが 1 件も拾わなかった場合は `.warning` に出る（exit は 0 のまま。フィルタで空になった木と、本当に空の会話は別物）
- **プラグインが Stop フックを同梱するようになった**。従来は `setup-hooks` の手動実行に依存しており、実行していない利用者では自動インデックスがほとんど走っていなかった
- `CONVERSATION_SEARCH_HOOK_TTL`: `hook` サブコマンド専用の TTL（既定 60 秒）。`search`/`tree`/`list` と同じ 300 秒のスタンプを共有していたため、セッション中に一度でも検索していると Stop フックが no-op になっていた。0 にしないのは、Stop がターン毎に発火するため常に stale だと毎ターン全ディレクトリを走査してしまうから

### Changed

- **破壊的変更: `tree --json` が既定で `full_content` を返さなくなった**。`--content` で復帰し、`--content-chars` で長さを制御する。従来は全ノードの本文を無条件に出しており、265 メッセージのセッションで数百 KB がエージェントの文脈に流れ込んでいた。`search` の `--content` オプトインと契約を揃える
- **`tree` が未インデックスのセッションを自動で索引するようになった**。ID が解決できないとき、そのセッションの transcript だけを同期で索引して 1 回引き直す。従来の自動インデックスはデタッチした別プロセスかつ 300 秒デバウンスのため、直前に終了したセッションには構造的に間に合わなかった。Claude Code のセッションのみが対象で、observer セッションは従来どおり除外される
- **サブエージェントの完了通知に `[Task notification] ` の接頭辞が付くようになった**。判定は Claude Code 自身の `origin.kind` による。潰さずタグ付けなのは、本文にエージェントの実際の結果が入っており有用なため。会話サマリの候補からは除外する（先頭 100 字が XML の定型部で、見出しが無意味になるため）
- `tree --flat` が時系列順に並ぶようになった。深さ優先のままだと、ルートが複数あるセッション（resume・sidechain・親が刈られた場合。実インデックスで 5,649 件）で古いメッセージが新しいものの後に来る。末尾 N 件を「直近」として読む使い方が壊れるため
- fzf プレビューが新フラグを使うようになった。従来は同等の処理を jq で手書きしていた
- **`tree` が失敗の原因を区別して伝えるようになった**。従来は「インデックスの破損・ロック」「曖昧なプレフィックス」「observer 等の意図的な除外」「ID の打ち間違い」がすべて `Conversation X not found` の 1 文に潰れており、直せる障害と入力ミスが見分けられなかった

### Added (開発者向け)

- `ACS_BINARY`: ラッパーが解決するバイナリのパスを上書きする。未公開バージョンのビルドに対して `tests/test_pick.sh` を走らせるために必要（従来はそのバージョンのリリースが存在せず、ダウンロードに失敗してテストが丸ごと skip されていた）

### Migration

- **既存インデックスには `[Task notification]` が付かない。再インデックスでも遡及しない**。`--force` が外すのは mtime スキップだけで、既にインデックス済みのセッションでは未登録 UUID のメッセージしか挿入されないため（`src/indexer/claude_code.rs` の `do_index_conversation`）。本文の導出ロジックを変えても既存行は書き換わらない。遡及したい場合は SQL を直接当てる（`messages_au` トリガが FTS を同期するので索引は保たれる）:

  ```sql
  UPDATE messages SET full_content = '[Task notification] ' || full_content
  WHERE message_type = 'user' AND full_content LIKE '<task-notification>%';
  ```
- **0.16.0 より前に `setup-hooks` を実行していてプラグインも導入している場合、`settings.json` 側の `ai-conversation-search hook` を削除してよい**。`setup-hooks` の冪等性チェックは `settings.json` しか見ないためプラグイン側のフックを検出できず、警告も出ないまま Stop が 2 回発火する。WAL なので破損はしないが索引プロセスが二重に走る
- スキルの最低バージョン要件が 0.16.0 に上がった

## [0.15.0] - 2026-08-10

利用中の AI エージェントから届いたフィードバックへの対応。「セッションを俯瞰してから深掘りする」動線が `tree` の不具合で塞がっていた点、検索結果が黙って打ち切られていた点が主。

### Added

- `prune-observer` サブコマンド: 過去にインデックスされた claude-mem observer セッションを削除する。`--dry-run` で件数と対象の一部を確認できる。実行時は確認を求め、stdin が端末でない場合（スクリプト・エージェント経由）は `--yes` がなければ拒否して終了する
- `search --content-chars N`: `--content` で表示する本文の文字数上限（既定 300）。`--content` は `--json` と `--group-by-session` でも効くようになった（JSON では各行に `full_content` と `full_content_truncated` が付く）
- `index --force`: 前回から変更のないファイルも読み直す。`--all` は日付範囲を広げるだけで、処理済みとして記録されたファイルには効かないため

### Changed

- **破壊的変更: `search` / `search --group-by-session` / `list` の `--json` が envelope になった**。従来の裸配列から `{"results": [...], "truncated": bool}` に変わる。打ち切り通知は stderr にしかなく、stdout だけを読む消費側は上限に当たった一覧を「これで全部」と読んでいたため。`.results[]` を読み、`.truncated` を確認すること。`tree` / `context` / `status` は元からオブジェクトで、変更なし。**長時間動いている Claude Code セッションは、旧形式の指示を保持している可能性があるため再起動を推奨**
- **`--json` の `resume_command` がシェルクォートされるようになった**。`cd -- '<path>' && claude --resume <id>` の形になる。空白を含むパスで `cd` が別の場所に落ち、`;` や `$(...)` を含むディレクトリ名なら `eval` 時に任意コマンドが走っていた。パスやセッション ID が安全に表現できない場合（制御文字を含む等）は `null` を返す
- **claude-mem observer セッションをインデックスしなくなった**。observer は一次セッションのツール実行を XML で複製したものと claude-mem が生成した観測ログからなり、どちらも本体は別の場所にある（前者は一次セッション、後者は `~/.claude-mem/claude-mem.db` の `observations` / `session_summaries`、いずれも検索可能）。実 DB では conversations の 78%（21,856/27,964）、messages の 29%（236,341/808,165）を占め、実ヒット 1 件につき複数の重複が付いていた。`CONVERSATION_SEARCH_INDEX_OBSERVER=1` と `--all --force` の併用で従来どおり取り込める
- **`--limit` で結果が打ち切られたとき stderr に通知するようになった**（`-v` の有無によらず）。従来は既定の 20 件で黙って切られており、「ヒットしなかった＝存在しない」と誤読する余地があった。FTS / LIKE フォールバックそれぞれの通常・`--group-by-session` の全 4 経路で検出する
- `search` と `list` の `--limit` に負値を渡すとエラーになる（従来は無制限として扱われていた）。`--group-by-session` 側は以前からエラー
- **`list` も `--limit` の打ち切りを通知するようになった**。従来は `search` にしか通知がなく、上限に当たった一覧が「これで全部」として読まれていた
- **`tree` の解決失敗が exit 1 になった**（破壊的変更）。従来は JSON・human いずれも exit 0 で、human 側はエラーを stdout に出していたため、`$?` を見るスクリプトが「曖昧で選べなかった」を「空の会話」と読んでいた。`.warning` は部分データが返っているので exit 0 のまま
- `CONVERSATION_SEARCH_INDEX_OBSERVER` が `1` 以外の一般的な真偽値（`true` / `yes` / `on` 等）も受理するようになった。認識できない値は stderr に警告する
- 複数行・空白のみの会話サマリが一覧の行レイアウトを壊さなくなった

### Fixed

- **`tree` のノード要約が常に空だった**。`messages.summary` はどのインデクサも書き込まないため、human 出力はアイコンだけの行が並ぶ状態だった。本文の先頭行から生成するようになり、raw トランスクリプト経由の表示と挙動が揃った
- **`tree` が短縮セッション ID を受け付けなかった**。一意な前方一致で解決するようになった。曖昧な場合は候補数を添えてエラーにする（黙って 1 件選ばない）。素の UUID から `oc:` / `codex:` 付きの ID にも解決する
- **FTS5 の DELETE / UPDATE トリガが削除済みメッセージを索引から除去できていなかった**。`message_content_fts` は外部コンテンツ表（`content='messages'`）で、この形では `DELETE FROM message_content_fts WHERE rowid = ?` は削除済みの行を読み直そうとして何も消せない（UPDATE 側は逆に、更新後の値で索引を消すため旧語が検索に残り続ける）。エラーにもならないため孤児エントリが黙って蓄積していた。`messages` の rowid は暗黙かつ再利用されるので、**孤児は最終的に無関係なメッセージを指し、検索がそれをヒットとして返す**。以後は正しい `'delete'` コマンドを使う（マイグレーション 9 で既存 DB のトリガも置き換わる）。既に蓄積した孤児は `prune-observer` の索引再構築で解消する（observer セッションが 0 件でも再構築だけは実行される）
- **`status` の FTS 健全性チェックが上記の孤児を検出できていなかった**。引数なしの `integrity-check` は索引内部の整合しか見ないため、`rank=1` を渡してコンテンツ表との突き合わせまで行うようにした。あわせて、DB を書き込みで開けないだけのケースを「破損」と報告しなくなり、失敗理由と次の一手を stderr に出すようにした
- **`--limit 0` のとき打ち切り通知が出なかった**。結果が空だと通知より先に「No results found」で return していたため、最も誤読しやすいケースで黙っていた
- Codex セッションのタイトルに非 ASCII 文字が含まれ、かつバイト位置 60 が文字の途中に当たると、インデックス実行全体が panic していた

### Note

- 既存 DB の observer 行は自動では削除されない。`prune-observer` を明示的に実行すること。数分かかり、不可逆で、事前のバックアップを推奨する。**ファイルサイズは縮まない**（解放されたページは以後のインデックスで再利用される）
- 破壊的処理をマイグレーションに載せていないのは、マイグレーションが `search` の起動するバックグラウンドインデクサ内で走るため。載せるとアップグレード後の最初の検索で 23 万行の削除が無言で始まり、バックアップを取る間もない

## [0.14.0] - 2026-08-03

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
