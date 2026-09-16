AGM 定期交辦：Claude Code 出新版了，請解析這一版有什麼**這個專案用得上**的東西，然後把結論送到使用者看得到的地方。

**通知怎麼送**：如果你是巡檢 AGM（使用者入口那顆），你這則回覆就是通知。如果你是協調者或其他 child（使用者看不到你的對話），解析完要用
`bin/agm assign --notice --bot <巡檢 bot id> --request-id agm-claude-release-<新版號>-notice --text '…'` 把結論交給巡檢，由它出現在使用者入口。

本次版本（由 `bin/claude-release-kick.sh` 填在訊息末尾）：舊版與新版的版本號，以及兩顆 binary 的路徑。

怎麼解析（唯讀，不要 build、不要重啟、不要改設定）：

1. CLI 介面差異：`diff <($OLD --help 2>&1) <($NEW --help 2>&1)`；有新子命令就再 `--help` 一次。
2. 設定與環境變數：兩顆 binary 各自抓 `describe("…")` 的欄位、`CLAUDE_CODE_*`／`CLAUDE_*` 變數名，取差集。例如：

   ```sh
   python3 - "$OLD" "$NEW" <<'PY'
   import re, sys
   def descs(p):
       d = open(p, 'rb').read()
       return set(m.group(1).decode('utf-8', 'replace') for m in re.finditer(rb'\.describe\("((?:[^"\\]|\\.){20,300})"\)', d))
   def envs(p):
       d = open(p, 'rb').read()
       return set(x.decode() for x in re.findall(rb'CLAUDE(?:_CODE)?_[A-Z0-9_]{3,40}', d))
   old, new = sys.argv[1], sys.argv[2]
   print('NEW FIELDS'); [print(' -', s[:240]) for s in sorted(descs(new) - descs(old))]
   print('NEW ENV'); print(sorted(envs(new) - envs(old)))
   PY
   ```
3. 找到疑似有用的功能就**實測**再下結論（例如新旗標就用 `-p` 跑一次、新輸出格式就看實際 JSON），不要只讀字串猜。測試一律用拋棄式目錄與 `-p`，不要動正式 daemon、不要在同機起第二顆 daemon。

判斷「用得上」的標準——這個專案目前靠這些吃飯，任何能讓它們更穩的都算：

- 額度：`quota_claude.rs` 讀 `/usage`（2.1.273 起已改吃 `usage_report` 結構化 JSON，舊版退回文字解析）、codex 狀態列、撞上限橫幅。
- 送達與回合：hook（SessionStart／Stop）、statusLine 回報、打字送達的證據、`--resume`／`--fork-session`／`codex fork`。
- 隔離與身分：`--settings`、`CLAUDE_CONFIG_DIR`、權限與 bypass、背景 session（`--bg`、`claude agents --json`）。
- 子 agent：`herdr` shim 帶下去的 env、subagent 相關旗標與設定。

回報（就是給使用者的通知，三到五行）：新版版本號、值得用的項目（每項一句：是什麼、對應我們哪個痛點、要改哪個檔）、明確沒用的略過不列、以及你建議的下一步（要不要派人改）。沒有值得用的就一句「本次無」。**不要**在這則交辦裡直接改程式或部署；要改另外走既有的派工與核准流程。
