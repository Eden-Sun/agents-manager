/**
 * 認出 codex 在 TUI 裡當場問的那個升級提示（2026-09-10 使用者需求：跟 claude 一樣，
 * 按下去之前先看新版改了什麼）。
 *
 * 畫面長這樣：
 * ```
 *   ✨ Update available! 0.153.4 -> 0.154.0
 *   Release notes: https://github.com/openai/codex/releases/...
 *   1. Update now (runs `sh -c 'curl -fsSL …'`)
 *   2. Skip
 *   3. Skip until next version
 * ```
 * 跟 claude 的差別是**新版還沒裝進磁碟**，所以目標版本只能從這句字讀出來，再交給
 * `GET /api/changelog?kind=codex&from=&to=`。
 */
export interface CodexUpdatePrompt {
  from: string | null
  to: string
}

const LINE = /update available!?\s*[:\s]\s*v?(\d+(?:\.\d+)+)\s*(?:->|→|=>)\s*v?(\d+(?:\.\d+)+)/i
/** 只帶新版沒帶舊版的寫法（`Update available! 0.154.0`）。 */
const ONLY_TO = /update available!?\s*[:\s]\s*v?(\d+(?:\.\d+)+)/i

/** 讀不出來就回 null——寧可不顯示，也不要拿錯版本去查 changelog。 */
export function parseCodexUpdatePrompt(text: string | null | undefined): CodexUpdatePrompt | null {
  if (!text) return null
  const both = LINE.exec(text)
  if (both) return { from: both[1], to: both[2] }
  const only = ONLY_TO.exec(text)
  return only ? { from: null, to: only[1] } : null
}
