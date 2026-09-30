# Claude binary diff 補充

只有訊息末尾附上 `Claude binary diff` 區塊時才做本節。它補充 changelog 看不到的本機 binary 變更，跟同一則 release-triage 交辦一起處理；**不要另派交辦、不要另送通知、不要直接跑 `gh`**。結論併進 release-triage 的同一則三到五行通知。

本次版本區塊提供舊版與新版路徑：

```text
OLD=<舊版路徑>
NEW=<新版路徑>
```

唯讀比較，不要 build、不要重啟、不要改設定：

1. CLI 介面差異：`diff <($OLD --help 2>&1) <($NEW --help 2>&1)`；有新子命令就再跑一次 `--help`。
2. 設定與環境變數：比較兩個 binary 的 `describe("…")` 欄位及 `CLAUDE_CODE_*`／`CLAUDE_*` 變數名。例如：

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
3. 疑似有用的功能先實測再下結論（例如新旗標用 `-p` 跑一次、新輸出格式檢查實際 JSON），不要只讀字串猜。測試用拋棄式目錄與 `-p`，不要動正式 daemon、不要在同機起第二顆 daemon。

判斷標準：額度（`quota_claude.rs` 的 `/usage`／`usage_report`、codex 狀態列、撞上限橫幅）；送達與回合（SessionStart／Stop、statusLine、打字送達證據、`--resume`／`--fork-session`／`codex fork`）；隔離與身分（`--settings`、`CLAUDE_CONFIG_DIR`、權限與 bypass、背景 session、`claude agents --json`）；子 agent（herdr shim 的 env、subagent 旗標與設定）。只把對這些實際路徑有影響的 binary 差異併入同一則通知。
