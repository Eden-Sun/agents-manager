import { stripMentionsOf } from '../api/mentions'

/** 群組輸入框的收件者前綴（chip 與 `@` 字串的互換）：純字串函式，給 `GroupChatPanel` 用。 */

export const MENTION_RE = /(^|[^\p{L}\p{N}_])@([^\s@,:;?!。，、！？()（）[\]{}<>"']+)/gu

/** 同一組字元集，但連同後面的空白一起吃掉（移除提及時不留下兩個空白）。 */
const MENTION_WITH_SPACE_RE = new RegExp(`${MENTION_RE.source}[ \\t]*`, 'gu')

/**
 * 拿掉**群組成員**（與 `@all`）的提及，前綴由 `applyRecipients` 重寫。內文不是成員的 `@字樣`（例如 `@alice`）原樣保留，
 * 內文的連續空白與換行也不動（以前整段壓成單一空白，切換 chip 就改寫了使用者打的內容）。
 */
export function stripMentions(text: string, names: string[]): string {
  // 名字有空白的先整段拿掉，不然 `@my bot` 只會拿掉 `@my`，`bot` 留在內文（2026-09-19）。
  const spaced = stripMentionsOf(text, names)
  const isMember = (tok: string) => {
    const bare = tok.replace(/[-_]+$/, '')
    return [tok, bare].some((c) => c.toLowerCase() === 'all' || names.some((n) => n.toLowerCase() === c.toLowerCase()))
  }
  return spaced.replace(MENTION_WITH_SPACE_RE, (m, pre: string, tok: string) => (isMember(tok) ? pre : m)).replace(/^[ \t]+/, '')
}

export function hasAllMention(text: string): boolean {
  for (const m of text.matchAll(MENTION_RE)) {
    if (m[2].toLowerCase() === 'all') return true
  }
  return false
}

export function applyRecipients(text: string, mode: 'all' | string[], names: string[]): string {
  const body = stripMentions(text, names)
  if (mode === 'all') return body ? `@all ${body}` : '@all '
  if (mode.length === 0) return body
  const prefix = mode.map((n) => `@${n}`).join(' ')
  return body ? `${prefix} ${body}` : `${prefix} `
}
