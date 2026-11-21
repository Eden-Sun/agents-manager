import { parseChoiceMenu } from './tuiChoices'

export interface CodexUpdatePrompt {
  from: string | null
  to: string
  interactive: boolean
}

const LINE = /update available!?\s*[:\s]\s*v?(\d+(?:\.\d+)+)\s*(?:->|→|=>)\s*v?(\d+(?:\.\d+)+)/i
const ONLY_TO = /update available!?\s*[:\s]\s*v?(\d+(?:\.\d+)+)/i
const ESC = String.fromCharCode(27)
const ANSI = new RegExp(`${ESC}\\[[0-?]*[ -/]*[@-~]|${ESC}\\][^\\x07${ESC}]*(?:\\x07|${ESC}\\\\)|${ESC}[ -/]*[0-~]`, 'g')
const UPDATE_NOW = /^\s*(?:[❯›▶>]\s*)?1[.)]\s+Update now\b/i
const SKIP = /^\s*(?:[❯›▶>]\s*)?2[.)]\s+Skip\b/i

function promptPrefix(line: string): number {
  const at = line.toLowerCase().indexOf('update available')
  if (at < 0 || /[\p{L}\p{N}]/u.test(line.slice(0, at))) return -1
  return at
}

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

import { msg } from './i18n'

export function isCodexUpdateMenu(text: string | null | undefined, key?: '1' | '2' | '3'): boolean {
  if (!parseCodexUpdatePrompt(text)?.interactive) return false
  const choices = parseChoiceMenu(text)?.choices ?? []
  const title = (number: number) => choices.find((choice) => choice.number === number)?.title ?? ''
  if (!/^Update now\b/i.test(title(1)) || !/^Skip\b/i.test(title(2))) return false
  return key !== '3' || /^Skip until next version\b/i.test(title(3))
}

export interface CodexUpdateIo {
  read: () => Promise<string | null>
  press: (keys: string[]) => void
}

export const CODEX_UPDATE_MOVED_ON = msg('畫面在這中間換過了，這一下沒有送出去——上面是重讀後的畫面，請再點一次。')

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
