/**
 * SPEC §13.2 mention rules, same as `daemon/src/group.rs::parse_mentions`: `@all` = everyone;
 * `@<name>` case-insensitive; `@` must start the text or follow a non-word char; trailing
 * `-` / `_` are retried without.
 */
export function parseMentions<T extends { name: string }>(text: string, members: T[]): T[] {
  const hits: T[] = []
  const re = /(^|[^\p{L}\p{N}_])@([^\s@,:;?!。，、！？()（）[\]{}<>"']+)/gu
  for (const m of text.matchAll(re)) {
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
