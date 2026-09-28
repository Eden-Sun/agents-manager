import { parseChoiceMenu } from './tuiChoices'

/**
 * 認出 codex TUI 的 `✨ Update available! 0.153.4 -> 0.154.0` 提示（2026-09-10 使用者需求：按之前先看
 * 改了什麼）。新版還沒裝進磁碟，目標版本只能從這句讀，再交給 `GET /api/changelog`。
 */
export interface CodexUpdatePrompt {
  from: string | null
  to: string
  /** 目前畫面是帶數字選項的互動提示；方框提示只供查看 changelog。 */
  interactive: boolean
}

const LINE = /update available!?\s*[:\s]\s*v?(\d+(?:\.\d+)+)\s*(?:->|→|=>)\s*v?(\d+(?:\.\d+)+)/i
const ONLY_TO = /update available!?\s*[:\s]\s*v?(\d+(?:\.\d+)+)/i
const ANSI = new RegExp(`${String.fromCharCode(27)}\\[[0-?]*[ -/]*[@-~]`, 'g')
const UPDATE_NOW = /^\s*(?:[❯›▶>]\s*)?1[.)]\s+Update now\b/i
const SKIP = /^\s*(?:[❯›▶>]\s*)?2[.)]\s+Skip\b/i

function promptPrefix(line: string): number {
  const at = line.toLowerCase().indexOf('update available')
  if (at < 0 || /[\p{L}\p{N}]/u.test(line.slice(0, at))) return -1
  return at
}

/** 互動選單可回答；安裝方框與只有新版號的提示都只提供 changelog 資訊。 */
export function parseCodexUpdatePrompt(text: string | null | undefined): CodexUpdatePrompt | null {
  if (!text) return null
  const lines = text.replace(ANSI, '').split(/\r?\n/)
  for (let i = 0; i < lines.length; i++) {
    const at = promptPrefix(lines[i])
    if (at < 0) continue
    const joined = `${lines[i].slice(at)} ${lines[i + 1] ?? ''}`
    const both = LINE.exec(joined)
    const only = both ? null : ONLY_TO.exec(joined)
    const from = both?.[1] ?? null
    const to = both?.[2] ?? only?.[1]
    if (!to) continue

    const next = lines.slice(i + 1, i + 9).map((line) => line.trim()).filter(Boolean)
    const boxed = next.slice(0, 3).some((line) => /\bto update\b/i.test(line) && /\b(?:install|run )/i.test(line))
    const interactive = !boxed && next.some((line) => UPDATE_NOW.test(line)) && next.some((line) => SKIP.test(line))
    if (interactive || boxed || !from) return { from, to, interactive }
  }
  return null
}

/** 確認目前焦點是 Codex 更新選單，且指定編號仍是該選單的選項。 */
export function isCodexUpdateMenu(text: string | null | undefined, key?: '1' | '2' | '3'): boolean {
  if (!parseCodexUpdatePrompt(text)?.interactive) return false
  const choices = parseChoiceMenu(text)?.choices ?? []
  const title = (number: number) => choices.find((choice) => choice.number === number)?.title ?? ''
  if (!/^Update now\b/i.test(title(1)) || !/^Skip\b/i.test(title(2))) return false
  return key !== '3' || /^Skip until next version\b/i.test(title(3))
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
  const screen = await io.read()
  const now = parseCodexUpdatePrompt(screen)
  if (
    !now ||
    now.to !== want.to ||
    (now.from ?? null) !== (want.from ?? null) ||
    !isCodexUpdateMenu(screen, key)
  ) return CODEX_UPDATE_MOVED_ON
  io.press([key])
  return null
}
