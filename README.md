# picocode

minimalist coding agent — Rust 製のミニマルな TUI コーディングエージェント

[rig](https://github.com/0xPlaygrounds/rig) のプロバイダ抽象と [ratatui](https://ratatui.rs) で構築。
ローカル LLM (Ollama) をデフォルトに、Anthropic / OpenAI にも切り替え可能。

## 機能

- **TUI チャット**: ストリーミング表示、スクロール(生成中も視点固定)、トークン使用量表示。モデルの思考ログはデフォルト折りたたみ(`Ctrl+T` で展開)
- **7 つの組み込みツール**: `read_file` / `list_files` / `grep` / `write_file` / `edit_file` / `bash` / `web_fetch`
- **承認フロー**: 破壊的操作(bash・ファイル書き込み)のみ y/n 確認。読み取り系は自動実行
- **マルチターン**: 会話履歴・ツール実行結果を保持したまま対話を継続
- **コンテキスト圧縮**: `/compact` で会話履歴を LLM 要約に置き換えてコンテキストを節約
- **設定ファイル**: `picocode.toml` で承認の allow/deny ルールと指示ファイル読み込みを設定

## セットアップ (ローカル LLM)

```sh
brew install ollama
brew services start ollama
ollama pull qwen3:4b
cargo run
```

## 使い方

```sh
picocode                                   # ollama/qwen3:4b (デフォルト)
picocode --model qwen3:8b                  # モデル変更
picocode --provider anthropic              # ANTHROPIC_API_KEY を使用
picocode --provider openai --model gpt-4o  # OPENAI_API_KEY を使用
picocode --yolo                            # 承認プロンプトを全てスキップ (危険)
```

TUI 内のキー操作:

| キー | 動作 |
|---|---|
| `Enter` | 送信 |
| `Tab` / `Shift+Tab` | コマンド補完(`/` 入力で候補ポップアップ、連打で循環) |
| `↑` / `↓` | 補完候補の選択 |
| `y` / `n` | ツール実行の承認 / 拒否 |
| `PgUp` / `PgDn` | スクロール(最下部まで戻ると追従再開) |
| `Ctrl+T` | 思考ログの展開 / 折りたたみ |
| `/clear` | 会話履歴をクリア |
| `/compact` | 会話履歴を要約に圧縮 |
| `/quit` (`Ctrl+C`) | 終了 |

## 設定ファイル

プロジェクト直下の `picocode.toml` を読み込む(グローバル設定
`~/.config/picocode/config.toml` があれば先に読み、プロジェクト側で上書き・追記)。

```toml
provider = "ollama"        # CLI 引数が優先
model = "qwen3:4b"

# 起動時にシステムプロンプトへ読み込む指示ファイル (デフォルト: ["AGENTS.md"])
instructions = ["AGENTS.md"]

[approval]
allow_tools = ["write_file"]      # 承認なしで実行するツール
deny_tools = ["web_fetch"]        # 常に自動拒否するツール (--yolo より優先)
allow_bash = ["cargo", "git status", "ls"]  # 承認なしで実行する bash コマンド
deny_bash = ["sudo", "rm -rf"]              # 常に自動拒否する bash コマンド
```

bash のルールはコマンドを `&&` `||` `;` `|` `&`・改行で分割し、各部分に**単語境界の前方一致**で適用する
(`cargo` は `cargo build` に一致、`cargofoo` には不一致。末尾の `*` は無視されるので `cargo *` とも書ける):

- `deny_bash`: どこか 1 箇所でも一致したら自動拒否(**`--yolo` でも拒否される**)
- `allow_bash`: **全ての**部分が一致した場合のみ承認なしで実行。コマンド置換 (`` ` `` や `$(`) を含む場合は自動実行しない
- どちらにも該当しなければ通常どおり y/n の承認プロンプト

## 構成

```
src/
  main.rs      — エントリポイント (+ --smoke ヘッドレスデバッグモード)
  config.rs    — CLI 引数・プロバイダ設定
  app.rs       — アプリ状態とイベントループ
  ui.rs        — ratatui 描画 (会話ログ / 入力欄 / ステータスバー / 承認モーダル)
  agent.rs     — rig Agent 構築とストリーミングワーカー
  approval.rs  — AgentHook による破壊的ツールの承認ゲート
  tools/       — 組み込みツール実装
```
