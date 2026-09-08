# Capture fixtures

這裡是 CLI 終端畫面的版本化樣本，供 `capture` parser runner 逐張比對。目錄固定為：

```text
<cli>/<version>/<name>.txt    # 原始（或去敏感資料後）終端畫面
<cli>/<version>/<name>.toml  # 同名畫面的期望值
```

目前的版本目錄是 `claude/2.1.263`、`codex/0.153.4`、`grok/1.0.13`。`.txt` 需保留 parser 會用到的原始 Unicode 字元（例如 spinner glyph、`⏺` / `•`、Grok 的框線）；可以移除 email、token、真實帳號與其他敏感路徑，但不要為了排版改動剩餘字元。

## TOML 欄位

每個 `.toml` 都必須有：

```toml
still_busy = false
awaits_input = true
```

可選欄位如下：

- `reply`：parser 應抽出的完整回覆；沒有這欄表示這張畫面不應產生回覆。
- `activity`：spinner 或工具進度的描述文字，不含開頭 glyph，例如 `Baking…` 或 `Running 1 shell command…`。
- `noise_lines`：應判定為 TUI 雜訊的 1-based `.txt` 行號陣列。
- `limit_hit`：限額／用量提示的完整文字；沒有這欄表示沒有提示。
- `source`：只有非真實畫面才必須寫 `source = "synthetic"`；真實 pane 快照可寫 `source = "herdr"`。
- `note`：說明來源、去敏感資料方式，或期望值與既有行為有意不同的原因。

`awaits_input` 僅表示 CLI 正在等使用者處理登入、權限、選單或其他確認；工作中的空 composer 不算等待使用者輸入。`reply` 與 `activity` 都是比對 parser 輸出的全文，包含段落換行。

## 新增 CLI 版本

1. 先執行該 CLI 的 `--version`，以實際輸出建立新的 `<version>` 目錄，不要覆寫舊版本。
2. 從 Herdr pane 取得 `recent_unwrapped`／visible snapshot，保留 spinner、框線與 marker；必要時才建立 synthetic 樣本，並在 TOML 加 `source = "synthetic"` 及 `note`。
3. 每種狀態新增 `.txt`／`.toml` pair，至少覆蓋 spinner、工具執行、完成回覆、marker 多段落、限額／用量及登入／權限確認；Grok 另保留 banner 樣本。
4. 跑 fixture runner；至少用 TOML parser 掃過此目錄，確認每個 pair 都存在且 `noise_lines` 沒有超出畫面行數。
