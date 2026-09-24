/**
 * 認出 codex TUI 的 `✨ Update available! 0.153.4 -> 0.154.0` 提示（2026-09-10 使用者需求：按之前先看
 * 改了什麼）。新版還沒裝進磁碟，目標版本只能從這句讀，再交給 `GET /api/changelog`。
 */
export interface CodexUpdatePrompt {
  from: string | null
  to: string
}

const LINE = /update available!?\s*[:\s]\s*v?(\d+(?:\.\d+)+)\s*(?:->|→|=>)\s*v?(\d+(?:\.\d+)+)/i
const ONLY_TO = /update available!?\s*[:\s]\s*v?(\d+(?:\.\d+)+)/i

/** 讀不出來回 null：寧可不顯示，也不拿錯版本查 changelog。 */
export function parseCodexUpdatePrompt(text: string | null | undefined): CodexUpdatePrompt | null {
  if (!text) return null
  const both = LINE.exec(text)
  if (both) return { from: both[1], to: both[2] }
  const only = ONLY_TO.exec(text)
  return only ? { from: null, to: only[1] } : null
}

/** 送出那一下要用的兩件事：重讀畫面、送鍵。 */
export interface CodexUpdateIo {
  /** 讀不到就回 `null`（當成畫面換過了，不送）。 */
  read: () => Promise<string | null>
  press: (keys: string[]) => void
}

export const CODEX_UPDATE_MOVED_ON = '畫面在這中間換過了，這一下沒有送出去——上面是重讀後的畫面，請再點一次。'

/**
 * 按 `1`／`2`／`3` 之前先重讀畫面，確認還停在**同一個**升級提示才送（issue #546）。
 *
 * 按鈕的依據是每秒輪詢的快照：提示已經被回答掉、或換成別的編號選項（codex 的核准框也是 `1.` 開頭）時，
 * 直接送那個數字會落進現在畫面上的東西。同一棵樹的 `BlockedChoices` 對選單就是這樣先 `read()` 再比對。
 *
 * 回 `null`＝送出了；回字串＝沒送出的原因。
 */
export async function answerCodexUpdate(io: CodexUpdateIo, want: CodexUpdatePrompt, key: '1' | '2' | '3'): Promise<string | null> {
  const now = parseCodexUpdatePrompt(await io.read())
  if (!now || now.to !== want.to || (now.from ?? null) !== (want.from ?? null)) return CODEX_UPDATE_MOVED_ON
  io.press([key])
  return null
}
