/**
 * 一顆 `busy` 旗標被兩層巢狀 async 操作共用時（例如「測試連線」內部先呼叫「儲存」），
 * 裡層自己的 `finally { setBusy(false) }` 會在外層還沒做完時就把旗標撥回 false——
 * 依賴這顆旗標 disable 的按鈕會在那個窗口放行，使用者能重複點擊、送出第二個併發請求
 * （`HostsPanel.tsx` 的 `RemoteCargoPanel`：「測試連線」借用「儲存」）。
 *
 * 用參照計數包一層：只有最外層的 `end()` 才真的把旗標撥回 false。
 */
export interface BusyLock {
  begin(): void
  end(): void
}

export function nestableBusy(setBusy: (busy: boolean) => void): BusyLock {
  let depth = 0
  return {
    begin() {
      depth++
      if (depth === 1) setBusy(true)
    },
    end() {
      if (depth === 0) return
      depth--
      if (depth === 0) setBusy(false)
    },
  }
}
