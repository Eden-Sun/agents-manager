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
