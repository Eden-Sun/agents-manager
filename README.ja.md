# AG Man（agents-manager）

[English](README.md) | [繁體中文](README.zh-TW.md) | [简体中文](README.zh-CN.md) | 日本語

**1 つの Web ページで coding agent チーム全体（Claude Code、Codex、Grok、Antigravity、API アドバイザー）を自分のマシン上で指揮。デスクトップ、iPad、スマートフォンのどこからでも操作できます。**

複数の coding agent CLI を同時に動かすと、端末タブの切り替えに追われ、1 時間も yes/no 確認で止まっているエージェントを見落とし、残クォータを推測する羽目になります。AG Man はすべての agent セッションを 1 つのセルフホスト Web UI にまとめ、対応が必要なときに通知し、bot 同士の連携もこなします。

<table>
  <tr>
    <td width="76%"><img src="docs/screenshots/readme/en/desktop-light.png" alt="デスクトップ画面：左にプロジェクトと bot 一覧、中央にチャット、上部にクォータバー、右に outbox" /></td>
    <td width="24%"><img src="docs/screenshots/readme/en/mobile-light.png" alt="スマートフォン画面：同期されたチャット、クォータチップ、bot 切り替え" /></td>
  </tr>
</table>

> スクリーンショットは内蔵モックバックエンド（`VITE_MOCK=1`）によるデモデータです。

## 仕組み

AG Man は軽量なローカル daemon（`agents-managerd`）と埋め込み Web UI で構成されます。CLI agent は [herdr](https://herdr.dev)（tmux のような端末ワークスペース管理ツール）の pane 内で動作し、daemon が起動・プロンプト送信・各 CLI のネイティブ hook 経由での応答受信を行います。画面スクレイピングではなく構造化イベントを受け取るため、きれいなメッセージやクリック可能な承認カードとして表示されます。API アドバイザーは API キーで daemon 内から直接動作し、端末は不要です。すべてのデータは自分のマシンに留まります。

AG Man は IDE ではありません。コードエディタ、diff レビュー、git worktree 管理は提供せず、各リポジトリの慣習に従って agent が扱います。

## 対応エージェント

| kind | 概要 |
|---|---|
| `claude` | Anthropic Claude Code CLI |
| `codex` | OpenAI Codex CLI |
| `grok` | xAI Grok CLI |
| `agy` | Google Antigravity CLI |
| `claude-api` | Anthropic API 直結（CLI 不要・アドバイザー専用） |
| `openai-api` | OpenAI API 直結（CLI 不要・アドバイザー専用） |

CLI bot はコマンド実行やファイル編集を行い、API bot は質問応答やセカンドオピニオンに特化（ターンごとのコスト上限設定が可能）しています。利用可能なモデルや effort レベルは各 CLI／プロバイダーから動的に取得され、対応 CLI では Web UI から即座に切り替えられます。各 CLI で複数アカウントに対応します。

## 主な機能

**端末を眺める代わりにチャットで操作**
- 各 CLI セッションをプロジェクト別のチャットとして一覧表示（実行中／完了／入力待ちが一目で分かります）。
- ターン実行中でも次のメッセージをキュー投入可能。確認や選択式の質問はクリックできるカードになります。
- 中断、巻き戻し（rewind）、文脈を引き継ぐ fork、生の端末タブへの切り替えがいつでも行えます。

**どのデバイスからでも状況を把握**
- デスクトップ、iPad（縦／横、Apple Pencil ホバープレビュー対応）、スマートフォンに対応。LAN や Tailscale 経由で接続できます。
- ターン完了、応答待ち、スケジュール結果、クォータリセットを Web Push で通知。すでに閲覧中のときは自動で抑止します。

**複数の bot をチームで動かす**
- **チーム役割（エキスパートモード）**：プロジェクトに管理者 bot を置き、ワーカーへの作業割り当てやアドバイザーへの相談を任せられます。
- **クラスタ**：同一構成のワーカーをまとめ、空いているメンバーに自動で振り分けます。
- **子 agent とスーパーバイザー**：herdr 経由で起動された子 agent は親の下に表示され、完了やスタックを親へ通知。任意のスーパーバイザー bot が全体を巡回しアイドル bot をスリープさせます。

**アカウント、クォータ、リモートホスト**
- 全アカウント・全ホストのサブスク残量、リセット時刻、Anthropic／OpenAI API 残高を上部の 1 本のバーに集約表示。
- Identities ページから分離アカウントの追加・ログイン、接続済みホスト間での同期が可能。
- SSH 先のリモートホスト上のプロジェクトでも、ローカルと同じく bot・shell・ログイン・クォータを扱えます。

**自動化とマシン管理**
- プロジェクトスケジュール（単発、一定間隔、毎日、毎週、複数時間帯）と実行履歴。
- 各ホストの herdr、agent CLI、hook をワンクリック更新。
- アイドル bot の自動スリープ＆文脈保持復帰、サイドバーでのメモリ・CPU 使用率表示。
- 他の agent を巻き込む広範囲な `pkill`/`killall` を内蔵ガードで遮断。

**ワークスペース補助機能**
- チャットの横にローカル開発サーバーのプレビューを表示（スマートフォン・iPad サイズ切り替え対応）。
- bot 間で共有する **File shelf** と、HTML ページ即時プレビュー・生成画像インライン表示を備えた **Outbox**。
- **共有リンク**：制限付きまたは信頼済み bot を専用 URL からチームメンバーへ共有可能。

英語・中国語 UI に対応（Settings → Display で切り替え）。詳細な仕様と制限は [`docs/SPEC.md`](docs/SPEC.md) をご覧ください。

## 対応プラットフォームと要件

- **macOS（arm64）**・**Linux（x86_64、arm64）**。Windows では **WSL2** 内で動かし、Windows のブラウザから開きます。
- **[herdr](https://herdr.dev)** — Homebrew では自動インストールされます。事前ビルドバイナリやソースビルドの場合は、先に [herdr.dev](https://herdr.dev) から `herdr` を導入してください。
- **上表のいずれか 1 つ以上の agent CLI または API キー**。
- ソースビルド時は Rust ツールチェーン（`cargo`）と [Bun](https://bun.sh) も必要です。

設定ページが各ホストを診断し、任意の補助ツール（`rtk`、`zsh`、`jq`、`gh`）が不足していれば案内します。

## クイックスタート

> **daemon や herdr を入れずに UI を先に試す：**
> ```bash
> git clone https://github.com/Eden-Sun/agents-manager.git
> cd agents-manager/web && bun install && VITE_MOCK=1 bun run dev
> ```

**方法 1：Homebrew**（macOS / Linux・`herdr` も同時インストール）

```bash
brew install eden-sun/tap/agents-manager
brew services start agents-manager    # 停止：brew services stop agents-manager
```

ログイン時に自動起動します。サービスから見える PATH は Homebrew の `bin` と `~/.local/bin` のみのため、agent CLI が別パスにある場合はシェルから `agents-managerd serve` を実行してください。

**方法 2：事前ビルドバイナリ**（Web UI 内蔵。リリースアーカイブには `agents-managerd` のみ含まれ `herdr` は同梱されないため、先に [herdr.dev](https://herdr.dev) から `herdr` を導入し `PATH` に通してください）。[Releases](https://github.com/Eden-Sun/agents-manager/releases/latest) から最新版を取得：

```bash
VERSION=v0.2.0          # Releases ページの最新タグに置き換えてください
ASSET=linux-x86_64      # または linux-arm64 / macos-arm64
curl -fLO "https://github.com/Eden-Sun/agents-manager/releases/download/$VERSION/agents-managerd-$VERSION-$ASSET.tar.gz"
tar -xzf "agents-managerd-$VERSION-$ASSET.tar.gz"
install -m 755 "agents-managerd-$VERSION-$ASSET/agents-managerd" ~/.local/bin/
agents-managerd serve   # ~/.local/bin が PATH に含まれていることを確認してください
```

macOS では初回のみ `xattr -d com.apple.quarantine ~/.local/bin/agents-managerd` が必要な場合があります。

**方法 3：ソースからビルド**（`herdr`・Rust・Bun が必要）

```bash
git clone https://github.com/Eden-Sun/agents-manager.git
cd agents-manager
(cd web && bun install && bun run build)        # Web UI をビルドして daemon に埋め込み
cargo build --release -p agents-managerd
./target/release/agents-managerd serve
```

起動後、ブラウザで **http://127.0.0.1:7788** を開きます（ローカルは自動サインイン。スマートフォンや別端末からは `http://<ホストIP>:7788` を開き `~/.config/agents-manager/ui-token` を貼り付けます）。**Add Project** → **Add Bot** を選べばチャットを始められます。

## データとセキュリティ

- 設定と履歴は `~/.config/agents-manager/` に保存されます（`AM_DATA_DIR` で変更可能）。リモートプロジェクトのデータは各ホストに残ります。
- 同ディレクトリの `ui-token` は Web UI の全権限を持つため、厳重に管理してください。
- `agents-managerd serve` を直接実行すると LAN／Tailscale 上の端末から接続できるよう全インターフェースで待ち受けます（ローカル限定にするには `AM_DEV_LAN=0`）。パッケージ版 macOS アプリは `127.0.0.1` のみで待ち受けます。
- 課金を伴う外部機能（Claude API、OpenAI API、メッセージ翻訳、共有機能）は明示的に有効化するまでオフです。
- Web Push は各ブラウザベンダーのプッシュサービスを経由します（本文は暗号化されますが、送信メタデータはサービス側から見えます）。

## 開発

```bash
scripts/check.sh changed    # origin/main との差分のみ検証
cargo dev                   # daemon (:7788) と Vite (:5173) を同時起動（停止は cargo down）
```

詳細は [`docs/FRONTEND.md`](docs/FRONTEND.md)、[`docs/AGENT-VERIFY.md`](docs/AGENT-VERIFY.md)、[`docs/BUILD-CACHE.md`](docs/BUILD-CACHE.md) を参照してください。

## ドキュメント

| ドキュメント | 内容 |
|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | 全体の動作仕様（bot、hook、ホスト、クォータ、アカウント、エキスパートモード、スケジュール） |
| [`docs/API.md`](docs/API.md) | HTTP / WebSocket API |
| [`docs/HOOK.md`](docs/HOOK.md) | 各 CLI の hook による応答通知 |
| [`docs/UI-DECISIONS.md`](docs/UI-DECISIONS.md) | 確定済みの UI 設計判断 |
| [`docs/PACKAGING.md`](docs/PACKAGING.md) | macOS `.dmg` のパッケージング |

## ライセンス

[MIT](LICENSE)
