/**
 * `GET /api/release-triage` 的分診帳本 → 更新框裡的「這幾版分析過了沒、結論是什麼」（issue #561）。
 *
 * 帳本早就在分析 claude／codex 每一版（SPEC §18.2c），只是網頁沒讀，使用者看不到。這裡只做投影：
 * 挑出 `(from, to]` 區間內的版本（`from` 不明就只看 `to`，跟 daemon 的 `pick_sections` 同一條規則），
 * 把逐條 verdict 分成提防／採用／值得早升，提案的 issue 帶標題與連結。
 */

export type TriageVerdict = 'guard' | 'adopt' | 'upgrade-arg' | 'none'

export interface TriageRow {
  version: string
  status: string
  entries: { id: string; text: string }[]
  verdicts: { entry_id: string; verdict: string; reason: string; module: string }[]
  proposals: { entry_ids: string[]; title: string; verdict: string; duplicate_of: number | null }[]
  issues: { entry_ids: string[]; number: number | null; url: string | null }[]
}

export interface TriageItem {
  id: string
  text: string
  reason: string
  module: string
}

export interface TriageIssue {
  title: string
  verdict: 'guard' | 'adopt' | ''
  number: number | null
  url: string | null
  /** 併進既有的 issue（只留言、不另開）。 */
  duplicate: boolean
}

export interface TriageVersion {
  version: string
  /** `judged`＝有結論（含已開 issue）；`empty`＝看過、沒有要處理的；`pending`＝排隊或分析中；`failed`；`missing`＝帳本沒有這一版。 */
  state: 'judged' | 'empty' | 'pending' | 'failed' | 'missing'
  guard: TriageItem[]
  adopt: TriageItem[]
  upgrade: TriageItem[]
  issues: TriageIssue[]
}

const isRec = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v)
const str = (v: unknown): string => (typeof v === 'string' ? v : '')
const strs = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [])
const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null)

/** 帳本一列。`verdicts` 存的是交回來的整份（`{verdicts:[…], issues:[…]}`）；舊資料或手寫的可能直接是陣列，兩種都收。 */
export function parseTriageRows(raw: unknown): { repo: string | null; rows: TriageRow[] } {
  const o = isRec(raw) ? raw : {}
  const rows = (Array.isArray(o.rows) ? o.rows : []).filter(isRec).map((r): TriageRow => {
    const v = r.verdicts
    const list = Array.isArray(v) ? v : isRec(v) && Array.isArray(v.verdicts) ? v.verdicts : []
    const props = isRec(v) && Array.isArray(v.issues) ? v.issues : []
    return {
      version: str(r.version),
      status: str(r.status),
      entries: (Array.isArray(r.entries) ? r.entries : []).filter(isRec).map((e) => ({ id: str(e.id), text: str(e.text) })),
      verdicts: list.filter(isRec).map((x) => ({ entry_id: str(x.entry_id), verdict: str(x.verdict), reason: str(x.reason), module: str(x.module) })),
      proposals: props.filter(isRec).map((p) => ({
        entry_ids: strs(p.entry_ids),
        title: str(p.title),
        verdict: str(p.triage) || str(p.verdict),
        duplicate_of: num(p.duplicate_of),
      })),
      issues: (Array.isArray(r.issues) ? r.issues : []).filter(isRec).map((i) => ({
        entry_ids: strs(i.entry_ids),
        number: num(i.number),
        url: str(i.url) || null,
      })),
    }
  })
  return { repo: str(o.repo) || null, rows }
}

function parts(v: string): number[] | null {
  if (!/^\d+(\.\d+)*$/.test(v)) return null
  return v.split('.').map(Number)
}

/** 數值比較（`0.9.0 < 0.10.0`）；看不懂的當成比誰都小。 */
export function cmpVersion(a: string, b: string): number {
  const x = parts(a)
  const y = parts(b)
  if (!x || !y) return (x ? 1 : 0) - (y ? 1 : 0)
  for (let i = 0; i < Math.max(x.length, y.length); i++) {
    const d = (x[i] ?? 0) - (y[i] ?? 0)
    if (d) return d
  }
  return 0
}

/** 模型寫的標題常自帶「（提防）／（採用）」：旁邊已經標了，拿掉。 */
function bareTitle(t: string): string {
  return t.replace(/[（(](提防|採用)[）)]\s*$/, '').trim()
}

function summarize(row: TriageRow, repo: string | null): TriageVersion {
  const text = new Map(row.entries.map((e) => [e.id, e.text]))
  const proposals = row.proposals.filter((p) => p.verdict === 'guard' || p.verdict === 'adopt')
  // 已經寫成提案的條目只列提案那一行（標題＋連結），不再逐條重複一次。
  const covered = new Set(proposals.flatMap((p) => p.entry_ids))
  const pick = (kind: TriageVerdict): TriageItem[] =>
    row.verdicts
      .filter((v) => v.verdict === kind && !covered.has(v.entry_id))
      .map((v) => ({ id: v.entry_id, text: text.get(v.entry_id) ?? '', reason: v.reason, module: v.module }))
  const issues = proposals
    .map((p): TriageIssue => {
      // 已開的 issue 以 entry_ids 對回提案（帳本的 `issues[]` 沒有標題）。
      const opened = row.issues.find((i) => i.entry_ids.some((id) => p.entry_ids.includes(id)))
      const number = opened?.number ?? p.duplicate_of
      const url = opened?.url ?? (number !== null && repo ? `https://github.com/${repo}/issues/${number}` : null)
      return { title: bareTitle(p.title), verdict: p.verdict as 'guard' | 'adopt', number, url, duplicate: !opened && p.duplicate_of !== null }
    })
  const state: TriageVersion['state'] =
    row.status === 'judged' || row.status === 'published'
      ? 'judged'
      : row.status === 'empty'
        ? 'empty'
        : row.status === 'failed'
          ? 'failed'
          : 'pending'
  return { version: row.version, state, guard: pick('guard'), adopt: pick('adopt'), upgrade: pick('upgrade-arg'), issues }
}

/**
 * `(from, to]` 區間內每一版的結論，新的在前。`to` 自己不在帳本裡時補一列 `missing`（「尚未分析」）；
 * 區間中間的版本帳本沒有就不知道它存不存在，不補。
 */
export function triageForRange(rows: TriageRow[], repo: string | null, from: string | null, to: string | null): TriageVersion[] {
  if (!to || !parts(to)) return []
  const inRange = rows.filter((r) => {
    if (!parts(r.version) || cmpVersion(r.version, to) > 0) return false
    return from && parts(from) ? cmpVersion(r.version, from) > 0 : cmpVersion(r.version, to) === 0
  })
  const out = inRange.map((r) => summarize(r, repo)).sort((a, b) => cmpVersion(b.version, a.version))
  if (!out.some((v) => cmpVersion(v.version, to) === 0)) {
    out.unshift({ version: to, state: 'missing', guard: [], adopt: [], upgrade: [], issues: [] })
  }
  return out
}

/** 整個區間有沒有任何一版分析出結果（`judged`／`empty`）。沒有＝「尚未分析」，更新框要把「請 AGM 解析」推到前面。 */
export function anyAnalysed(vs: TriageVersion[]): boolean {
  return vs.some((v) => v.state === 'judged' || v.state === 'empty')
}
