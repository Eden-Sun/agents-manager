# AG Man（agents-manager）

[English](README.md) | [繁體中文](README.zh-TW.md) | [简体中文](README.zh-CN.md) | 日本語

**すべての coding agent を、ひとつの Web ページで。**
Claude Code、Codex、Grok、Antigravity と API モデルを自分のマシンで動かし、デスクからでもスマートフォンからでも操作できます。

<table>
  <tr>
    <td width="75%"><img src="docs/screenshots/readme/en/desktop-light.png" alt="デスクトップ：サイドバーの bot は実行中・あなたの応答待ち・完了を表示、中央に回答待ちの選択式カード、上部に全アカウントのクォータ"></td>
    <td width="25%"><img src="docs/screenshots/readme/en/mobile-light.png" alt="スマートフォン：同じ選択式カードにワンタップで回答"></td>
  </tr>
</table>

<sub>スクリーンショットは組み込みのモックバックエンドで撮影しています。アカウント、bot、メッセージはデモデータです。UI は英語と繁体字中国語に対応しています。</sub>

複数の agent CLI を動かすと、ターミナルのタブを行き来し、yes/no の質問で 1 時間止まっている agent を見逃し、残りクォータを推測することになります。AG Man はすべての agent セッションをセルフホストの Web UI にまとめ、あなたの対応が必要なときに知らせ、agent 同士で作業を引き継がせます。すでに契約している CLI のサブスクリプションをそのまま使い、API キーは任意です。

- **止まった agent をスマートフォンから再開。** 完了や質問をプッシュ通知で受け取り、ワンタップで回答できます。
- **すべての agent と、あなたを待っているものが一目でわかる。** 各セッションはプロジェクトごとにまとまったチャットで、実行中・応答待ち・完了が表示されます。
- **全アカウントのクォータをまとめて確認。** 1 つの CLI に複数のサブスクリプションを使え、使用量・リセット時刻・Anthropic／OpenAI API 残高が 1 列に並びます。
- **bot をチームで動かす。** マネージャー bot がワーカー bot にタスクを渡し、アドバイザーにセカンドオピニオンを求めます。
- **手持ちのマシンをすべて使える。** プロジェクトを SSH ホストに置いても、ローカルと同じように使えます。

## 仕組み

AG Man は小さな daemon（`agents-managerd`）と Web UI です。agent CLI は [herdr](https://herdr.dev)（API を持つターミナルワークスペースマネージャー）の中で動きます。daemon が herdr を起動して操作するので、herdr を直接触る必要はありません。返信は各 CLI の hook 経由で届くため、画面から読み取った文字ではなく、本物のチャットメッセージとクリックできる質問カードとして表示されます。API bot はあなたの API キーで daemon 内で動き、ターミナルは不要です。

セッションはブラウザではなく daemon 側にあります。タブを閉じても agent は動き続け、どのデバイスから開いても続きから見られます。AG Man が管理するのは agent だけで、エディタ、git、リポジトリの慣習はそのままです。

## 対応する agent

| Agent | 説明 |
|---|---|
| Claude Code（`claude`） | Anthropic の coding CLI |
| Codex（`codex`） | OpenAI の coding CLI |
| Grok（`grok`） | xAI の coding CLI |
| Antigravity（`agy`） | Google の coding CLI |
| Claude API（`claude-api`） | あなたのキーで Anthropic API に直接接続。質問とセカンドオピニオン用で、ファイルは編集できません |
| OpenAI API（`openai-api`） | あなたのキーで OpenAI API に直接接続。質問とセカンドオピニオン用で、ファイルは編集できません |

モデルと推論強度は各 CLI やプロバイダーからリアルタイムに読み込まれ、CLI が対応していれば Web UI から切り替えられます。

## 機能

- **チャット：** ターン実行中に送ったメッセージはキューに入ります。中断、巻き戻し、bot の fork でコンテキストを引き継げます。生のターミナル画面もタブひとつで見られます。
- **チームロール（エキスパートモード）：** プロジェクト内で、マネージャー bot がエグゼキューター bot に作業を振り分け、アドバイザー bot に相談します。同じ設定のワーカーはまとまり、空いているものに作業が渡ります。bot が起動した子 agent はその下に表示され、完了や停止を親に知らせます。
- **スケジュール：** 1 回、一定間隔、毎日、毎週のプロンプト送信と実行履歴。
- **共有とファイル：** 権限を制限したリンクで bot を他の人に共有できます。bot が作ったファイルは outbox に入り、HTML は Web ページとして開け、Antigravity が生成した画像はチャットに表示されます。ファイルシェルフで bot 間のファイル受け渡しもできます。
- **プレビュー：** bot の dev server をチャットの横で、スマートフォンと iPad のサイズで確認できます。
- **アップデート：** すべてのホストで herdr と各 agent CLI をワンクリックで更新できます。
- **言語：** UI は英語と中国語に対応しています。

詳しい動作と制限は [`docs/SPEC.md`](docs/SPEC.md) を参照してください。

## プラットフォームと必要なもの

- **macOS（arm64）** または **Linux（x86_64、arm64）**。Windows では **WSL2** の中で動かし、Windows のブラウザから UI を開きます。
- **[herdr](https://herdr.dev)**。Homebrew なら自動でインストールされます。ビルド済みバイナリやソースからのビルドでは、先に自分でインストールしてください（`herdr --version` が動くこと）。
- 上の表の **agent CLI か API キーを少なくとも 1 つ**。
- ソースからビルドする場合は Rust ツールチェーンと [Bun](https://bun.sh) も必要です。

## クイックスタート

```bash
brew install eden-sun/tap/agents-manager    # macOS と Linux。herdr も一緒に入ります
brew services start agents-manager          # 今すぐ起動し、以降はログイン時に自動起動
```

**http://127.0.0.1:7788** を開きます。このマシンから開くと自動でサインインし、サイドバーが空で上部にクォータ列がある AG Man の画面が表示されます。

1. 各 agent CLI がログイン済みか確認します。ターミナルで一度起動するか、**Settings → Identities** からログインします。
2. **Add Project** をクリックし、ディレクトリ（このマシンまたは SSH ホスト上）を選びます。
3. プロジェクトの **+** メニューから **Add Bot** を選び、agent を選びます。
4. `hello` と送ります。返信がチャットの吹き出しで表示され、サイドバーの bot が実行中になります。

**スマートフォンや他のデバイス：** daemon はすべてのネットワークインターフェースで待ち受けるので、同じ LAN や Tailscale 上のスマートフォンから `http://<このマシンのアドレス>:7788` を開き、`~/.config/agents-manager/ui-token` の内容を貼り付けてサインインできます。`AM_DEV_LAN=0` を設定するとローカル接続のみになります。

**CLI が見つからない場合：** Homebrew のサービスは Homebrew の `bin` と `~/.local/bin` しか見えません。サービスを止め（`brew services stop agents-manager`）、自分のシェルから `agents-managerd serve` を実行してください。ログは `$(brew --prefix)/var/log/agents-manager.log` にあります。

<details>
<summary><b>その他のインストール方法：ビルド済みバイナリ、ソースからのビルド</b></summary>

どちらも先に [herdr](https://herdr.dev) のインストールが必要です。

**ビルド済みバイナリ**（Web UI 内蔵。`linux-x86_64`、`linux-arm64`、`macos-arm64`）：

```bash
ASSET=linux-x86_64      # または linux-arm64 / macos-arm64
VERSION=$(curl -fsSL https://api.github.com/repos/Eden-Sun/agents-manager/releases/latest | grep -m1 '"tag_name"' | cut -d'"' -f4)
curl -fLO "https://github.com/Eden-Sun/agents-manager/releases/download/$VERSION/agents-managerd-$VERSION-$ASSET.tar.gz"
tar -xzf "agents-managerd-$VERSION-$ASSET.tar.gz"
mkdir -p ~/.local/bin && install -m 755 "agents-managerd-$VERSION-$ASSET/agents-managerd" ~/.local/bin/
~/.local/bin/agents-managerd serve
```

各アーカイブには [Releases](https://github.com/Eden-Sun/agents-manager/releases/latest) ページに検証用の `.sha256` があります。macOS ではダウンロードしたバイナリに一度 `xattr -d com.apple.quarantine ~/.local/bin/agents-managerd` が必要な場合があります。

**ソースからビルド：**

```bash
git clone https://github.com/Eden-Sun/agents-manager.git
cd agents-manager
(cd web && bun install && bun run build)        # Web UI は daemon に埋め込まれます
cargo build --release -p agents-managerd
./target/release/agents-managerd serve
```

常駐させるには systemd user unit、launchd、または herdr pane を使います。

</details>

**まず見てみたい場合：** リポジトリを `git clone` して `cd web && bun install && VITE_MOCK=1 bun run dev` を実行すると、デモデータで UI を開けます。daemon も herdr も不要です。

## データとセキュリティ

- 設定と履歴は `~/.config/agents-manager/` にあります（`AM_DATA_DIR` で変更可）。リモートプロジェクトのデータはそのホストに残ります。
- このディレクトリの `ui-token` は UI の完全な操作権限です。外部に漏らさないでください。
- パッケージ版の macOS アプリは `127.0.0.1` のみで待ち受けます。`agents-managerd serve` は `AM_DEV_LAN=0` を設定しない限りすべてのインターフェースで待ち受けます。
- サードパーティや外部公開に関わる機能（Claude API、OpenAI API、翻訳、共有リンク）は、自分で有効にするまでオフです。
- プッシュ通知はブラウザベンダーのプッシュサービスを経由します。内容は暗号化されますが、通知が送られた時刻はサービス側から見えます。

## 開発

```bash
scripts/check.sh changed    # origin/main からの変更部分だけを検証
cargo dev                   # daemon（:7788）＋ Vite dev server（:5173）。停止は cargo down
```

[`docs/FRONTEND.md`](docs/FRONTEND.md)、[`docs/AGENT-VERIFY.md`](docs/AGENT-VERIFY.md)、[`docs/BUILD-CACHE.md`](docs/BUILD-CACHE.md) も参照してください。

## ドキュメント

| ドキュメント | 内容 |
|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | 全体の動作仕様：bot、hook、ホスト、クォータ、アイデンティティ、チームロール、スケジュール |
| [`docs/API.md`](docs/API.md) | HTTP／WebSocket API |
| [`docs/HOOK.md`](docs/HOOK.md) | 各 CLI の hook による返信の報告方法 |
| [`docs/UI-DECISIONS.md`](docs/UI-DECISIONS.md) | 確定済みの UI 上の判断 |
| [`docs/PACKAGING.md`](docs/PACKAGING.md) | macOS `.dmg` のパッケージング |

## ライセンス

[MIT](LICENSE)
