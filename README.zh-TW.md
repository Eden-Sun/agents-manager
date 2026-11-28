# AG Man（agents-manager）

[English](README.md) | 繁體中文 | [简体中文](README.zh-CN.md) | [日本語](README.ja.md)

**一個網頁，同時指揮整支 coding agent 團隊——Claude Code、Codex、Grok、Antigravity 與 API 顧問——跑在你自己的機器上，桌機、iPad、手機都能隨時操作。**

同時開多個 coding agent CLI，往往要在一堆終端分頁間切換、漏掉卡在 yes/no 確認題一小時的那一顆，還得猜各帳號剩多少額度。AG Man 把所有 agent 工作階段收進同一個自架網頁介面，需要你介入時立刻通知，還能讓多顆 bot 互相協作。

<table>
  <tr>
    <td width="76%"><img src="docs/screenshots/readme/en/desktop-light.png" alt="桌機畫面：左側專案與 bot 列表、中間對話區、頂部額度列、右側 outbox" /></td>
    <td width="24%"><img src="docs/screenshots/readme/en/mobile-light.png" alt="手機畫面：同步對話、額度晶片與 bot 切換" /></td>
  </tr>
</table>

> 截圖使用內建假資料後端（`VITE_MOCK=1`）；帳號、bot 與訊息皆為示範資料。

## 運作方式

AG Man 由一個輕量本機 daemon（`agents-managerd`）與內嵌網頁介面組成。CLI agent 跑在 [herdr](https://herdr.dev)（類似 tmux 的終端工作區管理器）的 pane 裡；daemon 負責啟動、送出訊息，並透過各 CLI 原生 hook 接收回覆——拿到的是乾淨的結構化訊息與可點擊的確認卡片，而不是從終端畫面爬字。API 顧問則直接用 API key 在 daemon 內執行，不需終端。所有資料都留在你自己的機器上。

AG Man 不是 IDE：不提供程式碼編輯器、diff 檢視或 git worktree 管理，這些交由 agent 依各 repo 慣例自行處理。

## 支援的 agent

| kind | 說明 |
|---|---|
| `claude` | Anthropic Claude Code CLI |
| `codex` | OpenAI Codex CLI |
| `grok` | xAI Grok CLI |
| `agy` | Google Antigravity CLI |
| `claude-api` | 直連 Anthropic API（不需 CLI；擔任顧問） |
| `openai-api` | 直連 OpenAI API（不需 CLI；擔任顧問） |

CLI bot 可執行指令與編輯檔案；API bot 專責問答與第二意見，可設每回合成本上限。可用模型與 effort 等級會從各 CLI 或供應商即時讀取，CLI 支援時可直接在網頁切換。每種 CLI 皆支援多帳號。

## 主要功能

**用對話取代盯終端機**
- 每個 CLI 工作階段都變成按專案分組的聊天室，狀態一目瞭然（執行中、完成、等你回應）。
- 回合執行中可直接排隊下一則訊息；確認與多選題自動轉成可點擊卡片。
- 隨時中斷、倒回（rewind）、fork 接續脈絡，或一鍵切回原始終端分頁。

**任何裝置都掌握進度**
- 支援桌機、iPad（直／橫向，支援 Apple Pencil 懸停預覽）與手機，區網或 Tailscale 皆可連。
- 網頁推播（Web Push）通知回合完成、等待回答、排程結果與額度重置；你正在看畫面時自動靜音。

**讓多顆 bot 組隊協作**
- **團隊角色（專家模式）**：為專案指派管理者 bot，由它派工給執行者並諮詢顧問。
- **群集派工**：將同規格執行者組成群集，自動分配給閒置成員。
- **子 agent 與總管**：透過 herdr 開出的子 agent 會縮排掛在父 bot 下，完成或卡住時自動回報；另可選配總管 bot 巡檢並收起閒置 bot。

**帳號、額度與遠端主機**
- 頂部單一額度列即時顯示各帳號、各主機的訂閱額度、重置倒數與 Anthropic／OpenAI API 餘額。
- 在「身份（Identities）」頁新增與登入隔離帳號，並支援跨已連線主機同步身份。
- 專案可放在遠端 SSH 主機，享有相同的 bot、shell、登入與額度檢視。

**自動化與主機健康**
- 專案排程（單次、間隔、每日、每週、多時段）並保留執行紀錄。
- 單鍵更新每台主機上的 herdr、各 agent CLI 與 hook。
- 閒置 bot 自動休眠並保留恢復脈絡；側欄即時顯示各主機記憶體與 CPU 使用率。
- 內建防護攔截廣域 `pkill`/`killall`，避免誤殺其他 agent。

**工作區加值工具**
- 對話旁直接預覽 bot 的本機 dev server，支援切換手機與 iPad 尺寸。
- 跨 bot **檔案暫存**與每顆 bot 的 **Outbox**，支援 `.html` 一鍵網頁預覽與產圖內嵌聊天。
- **分享連結**：透過獨立入口將受限或受信任的 bot 分享給團隊成員使用。

提供中英文介面（於「設定 → Display」切換）。完整行為與限制見 [`docs/SPEC.md`](docs/SPEC.md)。

## 支援平台與需求

- **macOS（arm64）**與 **Linux（x86_64、arm64）**。Windows 請在 **WSL2** 內執行，由 Windows 瀏覽器開啟網頁。
- **[herdr](https://herdr.dev)**——Homebrew 會自動安裝；若使用預編譯執行檔或從原始碼建置，請先從 [herdr.dev](https://herdr.dev) 安裝 `herdr`。
- **至少一個上表支援的 agent CLI 或 API key**。
- 從原始碼建置另需 Rust 工具鏈（`cargo`）與 [Bun](https://bun.sh)。

設定頁會自動檢查各主機環境，缺少選用輔助工具（`rtk`、`zsh`、`jq`、`gh`）時主動提示。

## 快速開始

> **想先看介面、不裝 daemon 或 herdr：**
> ```bash
> git clone https://github.com/Eden-Sun/agents-manager.git
> cd agents-manager/web && bun install && VITE_MOCK=1 bun run dev
> ```

**方式一：Homebrew**（macOS 與 Linux，會自動安裝 `herdr`）

```bash
brew install eden-sun/tap/agents-manager
brew services start agents-manager    # 停止：brew services stop agents-manager
```

服務會於登入時自動啟動，但只看得到 Homebrew 的 `bin` 與 `~/.local/bin`；若你的 agent CLI 裝在其他位置，請改在 shell 執行 `agents-managerd serve`。

**方式二：預編譯執行檔**（已內嵌網頁介面；release 封存檔僅含 `agents-managerd`、不含 `herdr`，請先從 [herdr.dev](https://herdr.dev) 安裝 `herdr` 並確認在 `PATH` 中）。從 [Releases](https://github.com/Eden-Sun/agents-manager/releases/latest) 下載最新版：

```bash
VERSION=v0.2.0          # 請改為 Releases 頁最新版號
ASSET=linux-x86_64      # 或 linux-arm64 / macos-arm64
curl -fLO "https://github.com/Eden-Sun/agents-manager/releases/download/$VERSION/agents-managerd-$VERSION-$ASSET.tar.gz"
tar -xzf "agents-managerd-$VERSION-$ASSET.tar.gz"
install -m 755 "agents-managerd-$VERSION-$ASSET/agents-managerd" ~/.local/bin/
agents-managerd serve   # 請確認 ~/.local/bin 在 PATH 中
```

macOS 下載的執行檔首次執行可能需要 `xattr -d com.apple.quarantine ~/.local/bin/agents-managerd`。

**方式三：從原始碼建置**（需先安裝 `herdr`、Rust 與 Bun）

```bash
git clone https://github.com/Eden-Sun/agents-manager.git
cd agents-manager
(cd web && bun install && bun run build)        # 建置網頁並內嵌至 daemon
cargo build --release -p agents-managerd
./target/release/agents-managerd serve
```

啟動後開啟 **http://127.0.0.1:7788**（本機自動登入；手機或其他裝置請開 `http://<主機 IP>:7788` 並貼上 `~/.config/agents-manager/ui-token`）。按 **新增 Project**、再按 **新增 Bot** 即可開始對話。

## 資料與安全

- 設定與歷史紀錄存於 `~/.config/agents-manager/`（可用 `AM_DATA_DIR` 覆寫）；遠端專案資料留在各自遠端主機。
- 該目錄內的 `ui-token` 擁有完整網頁權限，請妥善保管。
- 直接執行 `agents-managerd serve` 預設監聽所有介面以供區網／Tailscale 手機連線；若只限本機可設 `AM_DEV_LAN=0`。打包版 macOS app 僅監聽 `127.0.0.1`。
- 付費第三方功能（Claude API、OpenAI API、訊息翻譯、分享入口）預設全部關閉，需手動啟用。
- 網頁推播經由各瀏覽器廠商推播服務傳遞（內容加密，但推播服務可見發送時間與對象）。

## 開發

```bash
scripts/check.sh changed    # 只驗證相對於 origin/main 的改動
cargo dev                   # 同時啟動 daemon (:7788) 與 Vite (:5173)，cargo down 停止
```

詳見 [`docs/FRONTEND.md`](docs/FRONTEND.md)、[`docs/AGENT-VERIFY.md`](docs/AGENT-VERIFY.md) 與 [`docs/BUILD-CACHE.md`](docs/BUILD-CACHE.md)。

## 文件索引

| 文件 | 內容 |
|---|---|
| [`docs/SPEC.md`](docs/SPEC.md) | 完整行為規格：bot、hook、主機、額度、身份、專家模式、排程 |
| [`docs/API.md`](docs/API.md) | HTTP／WebSocket API |
| [`docs/HOOK.md`](docs/HOOK.md) | 各 CLI hook 回報機制 |
| [`docs/UI-DECISIONS.md`](docs/UI-DECISIONS.md) | 已定案的介面取捨 |
| [`docs/PACKAGING.md`](docs/PACKAGING.md) | 打包 macOS `.dmg` |

## 授權

[MIT](LICENSE)
