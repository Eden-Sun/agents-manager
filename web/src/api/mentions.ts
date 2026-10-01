/** SPEC §13.2 mention rules; must match `daemon/src/group.rs::parse_mentions`. */
const MENTION_CHAR = /[^\s@,:;?!。，、！？()（）[\]{}<>"']/u

/**
 * `@` 後面正好接著某個**名字有空白**的成員整個名字（後面是結尾或非名字字元）就是它，最長的先比；
 * 同 daemon `group::spaced_member_at`（2026-09-19 使用者：名字可以有空白）。回名字結束位置（字串索引）。
 * 大小寫完全相同的優先；沒有完全相同、而且（同一個結束位置上）摺完只有一顆時才不分大小寫（#657）。
 */
function spacedNameAt(text: string, start: number, names: string[]): { name: string; end: number } | null {
  let exact: { name: string; end: number } | null = null
  let fold: { name: string; end: number } | null = null
  let foldTie = false
  for (const name of names) {
    if (!name.includes(' ')) continue
    const end = start + name.length
    if (end > text.length) continue
    const next = text[end]
    if (next !== undefined && MENTION_CHAR.test(next) && next !== '-' && next !== '_') continue
    const typed = text.slice(start, end)
    if (typed === name) {
      if (!exact || end > exact.end) exact = { name, end }
    } else if (typed.toLowerCase() === name.toLowerCase()) {
      if (fold && end === fold.end) foldTie = true
      else if (!fold || end > fold.end) {
        fold = { name, end }
        foldTie = false
      }
    }
  }
  return exact ?? (foldTie ? null : fold)
}

/** daemon 的 `token_member`：大小寫完全相同的先；沒有時只摺 ASCII，摺完恰好一顆才算（兩顆只差大小寫就誰都不算，#657）。 */
function tokenMember<T extends { name: string }>(raw: string, members: T[]): T | null {
  const exact = members.find((x) => x.name === raw)
  if (exact) return exact
  const ascii = (s: string) => s.replace(/[A-Z]/g, (c) => c.toLowerCase())
  const folded = ascii(raw)
  let hit: T | null = null
  for (const x of members) {
    if (ascii(x.name) !== folded) continue
    if (hit) return null
    hit = x
  }
  return hit
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
    const raw = m[2]
    for (const cand of [raw, raw.replace(/[-_]+$/, '')]) {
      if (!cand) continue
      if (cand.toLowerCase() === 'all') return [...members]
      const hit = tokenMember(cand, members)
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
