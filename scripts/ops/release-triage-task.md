AGM 定期交辦：上游（claude／codex）出新版了，請對訊息末尾那份 JSON 裡**每一條** kept／unmatched 的 changelog 條目下 verdict，該處理的開成 issue（由 daemon 開，你不直接跑 `gh`）。只分診，**不改程式、不升級、不改設定、不重啟**。

**通知怎麼送**：如果你是巡檢 AGM（使用者入口那顆），你這則回覆就是通知。如果你是協調者或其他 child（使用者看不到你的對話），做完要用
`bin/agm assign --notice --bot <巡檢 bot id> --request-id agm-claude-release-<新版號>-notice --text '…'` 把結論交給巡檢，由它出現在使用者入口。（`<新版號>` 換成本次 JSON 的 `to`；codex 那一輪一樣用這個格式。）

## 輸入

訊息末尾的 JSON：`{"kind","from","to","pending":[{"version","kept":[{"id","text","categories":[]}],"unmatched":[{"id","text"}],"dropped_count"}]}`。
`kept` 是規則命中的條目（`categories` 是命中的類別）；`unmatched` 是兩邊都沒中的，**照樣要判**（規則不夠好時東西不能靜靜消失）；`dropped_count` 是規則已丟掉的條數，不用管。
若有 `deferred_versions`，那些版留給下一輪，這則不要碰。

## verdict 四類

每個 kept／unmatched 的 `id` 都必須有一個 verdict，缺一條整份會被退回：

- `guard`：不處理會壞、或行為會變（提防）。例：被移除／改語意的旗標、hook 事件、resume 行為、輸入框或確認框的畫面文字。
- `adopt`：處理了會更好（採用）。例：新的結構化輸出、新的 hook、可以取代畫面解析的設定。
- `upgrade-arg`：只是「值得早點升級」的理由，**不用改程式**（不開 issue，列進通知，AGM 排升級時參考）。
- `none`：跟這個系統無關，或對得到的模組找不到影響。「認為有關但找不到對應模組」＝ `none`，理由照寫。

**只有 `guard`／`adopt` 會變成 issue。** 每個 verdict 附一句理由（`reason`）與「對到我們哪個模組／哪個檔」（`module`；`none` 可留空字串）。

## 模組對照表

| 主題 | 模組 |
| --- | --- |
| 額度 | `quota_claude.rs`、`quota.rs` |
| 送達（打字、prompt） | `lifecycle/delivery.rs`、`prompt.rs` |
| 注入設定（`--settings`、env） | `lifecycle/setup.rs` |
| hook | `hookrecv.rs`、`hook_cmd.rs` |
| 畫面判讀 | `lifecycle/screen.rs`、`tui_prompts.rs`、`codex_live.rs` |
| resume／fork | `lifecycle/start.rs`、`bulk_restart.rs` |
| 子 agent | `herdr_shim.rs`、`reconcile.rs` |

## 怎麼查（控制成本）

- 開 issue 之前先 `gh issue list --label release-triage --state all -L 30`，看有沒有**同主題**的（已關的也算：關掉＝做完或判定不做）。有就在該 issue 的 `duplicate_of` 填它的編號，daemon 只會留言、不另開。
- **每條最多查證一次**；**不要為了 `none` 的條目讀檔**。要驗證就 grep 對應模組，不要通讀。一則交辦預算約 60k tokens、一回合做完。
- 同一原因的多條可以合併成一張 issue，`entry_ids` 列全部。
- `## 來源` 的引用由 daemon 從帳本貼原文，**你不用也不要在 issue 內文裡重打 changelog 原文**。

## 交回結果

寫一份 `verdicts.json`，用 CLI 交回（daemon 驗過才開；不要自己跑 `gh issue create`／`gh issue comment`）：

```sh
bin/agm release-triage submit --file verdicts.json
```

```json
{
  "verdicts": [
    {"entry_id": "a1b2c3d4e5", "verdict": "guard", "reason": "一句話理由", "module": "lifecycle/screen.rs"}
  ],
  "issues": [
    {
      "entry_ids": ["a1b2c3d4e5"],
      "verdict": "guard",
      "title": "一句話：發生什麼事、影響我們哪裡（不要自己寫 `claude 2.1.280:` 這種前綴，daemon 會貼）",
      "goal": "## 目標 的內容",
      "suggestion": "## 建議 的內容：要改哪個檔、怎麼改",
      "acceptance": "## 驗收 的內容：怎麼證明處理好了",
      "duplicate_of": 102
    }
  ]
}
```

- 一份 JSON 涵蓋本次所有版本：每個 `entry_id` 本來就唯一。欄位細節若跟 `bin/agm release-triage submit --help` 不一致，以 CLI／`docs/API.md` 為準。
- `duplicate_of` 只有找到同主題 issue 時才填，沒有就整個欄位省略。
- `title` 只寫那一句話：完整標題是 daemon 組的 `<kind> <version>: <你的一句話>（提防｜採用）`，
  自己再寫一次版本前綴會被剝掉（別版的前綴剝不掉，會變成兩層），結尾也不用自己加「（提防）」。
- `issues` 只放 `guard`／`adopt`；沒有就給空陣列 `[]`（該版會標為 `empty`，不開 issue）。

## 通知（三到五行）

做完送一則三到五行的通知：這次看了哪個 kind、哪幾版；開了幾張 issue（編號與一句話）；`upgrade-arg` 清單（每項一句：是什麼、為什麼值得早升）——沒有就一句「`upgrade-arg`：本次無」。整體沒有 `guard`／`adopt` 就一句「本次無」。**不要**在這則交辦裡直接改程式或部署；要改另外走既有的派工與核准流程。
