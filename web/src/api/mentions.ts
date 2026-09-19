/** SPEC §13.2 mention rules; must match `daemon/src/group.rs::parse_mentions`. */
const MENTION_CHAR = /[^\s@,:;?!。，、！？()（）[\]{}<>"']/u

/**
 * `@` 後面正好接著某個**名字有空白**的成員整個名字（不分大小寫、後面是結尾或非名字字元）就是它，最長的先比；
 * 同 daemon `group::spaced_member_at`（2026-09-19 使用者：名字可以有空白）。回名字結束位置（字串索引）。
 */
function spacedNameAt(text: string, start: number, names: string[]): { name: string; end: number } | null {
  let best: { name: string; end: number } | null = null
  for (const name of names) {
    if (!name.includes(' ')) continue
    const end = start + name.length
    if (end > text.length || text.slice(start, end).toLowerCase() !== name.toLowerCase()) continue
    const next = text[end]
    if (next !== undefined && MENTION_CHAR.test(next) && next !== '-' && next !== '_') continue
    if (!best || end > best.end) best = { name, end }
  }
  return best
}

const isBoundary = (text: string, at: number) => at === 0 || !/[\p{L}\p{N}_]/u.test(text[at - 1])

export function parseMentions<T extends { name: string }>(text: string, members: T[]): T[] {
  const hits: T[] = []
  const names = members.map((m) => m.name)
  const re = /(^|[^\p{L}\p{N}_])@([^\s@,:;?!。，、！？()（）[\]{}<>"']+)/gu
  // 先收有空白的名字，並記下它們佔掉的範圍，免得 `@my bot` 又被下面的逐字比對當成 `@my`。
  const covered: Array<[number, number]> = []
  for (let i = text.indexOf('@'); i !== -1; i = text.indexOf('@', i + 1)) {
    if (!isBoundary(text, i)) continue
    const hit = spacedNameAt(text, i + 1, names)
    if (!hit) continue
    const m = members.find((x) => x.name === hit.name)
    if (m && !hits.includes(m)) hits.push(m)
    covered.push([i, hit.end])
  }
  for (const m of text.matchAll(re)) {
    const at = (m.index ?? 0) + m[1].length
    if (covered.some(([a, b]) => at >= a && at < b)) continue
    const raw = m[2].toLowerCase()
    for (const cand of [raw, raw.replace(/[-_]+$/, '')]) {
      if (!cand) continue
      if (cand === 'all') return [...members]
      const hit = members.find((x) => x.name.toLowerCase() === cand)
      if (hit) {
        if (!hits.includes(hit)) hits.push(hit)
        break
      }
    }
  }
  return members.filter((x) => hits.includes(x))
}

/** 把 `@<有空白的名字>` 整段拿掉（群組收件人 chip 重寫前綴用）；其餘 `@token` 交給呼叫端原本的規則。 */
export function stripMentionsOf(text: string, names: string[]): string {
  let out = ''
  let i = 0
  while (i < text.length) {
    if (text[i] === '@' && isBoundary(text, i)) {
      const hit = spacedNameAt(text, i + 1, names)
      if (hit) {
        i = hit.end
        continue
      }
    }
    out += text[i]
    i += 1
  }
  return out
}
