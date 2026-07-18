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
