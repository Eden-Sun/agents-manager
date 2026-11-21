
export type TriageVerdict = 'guard' | 'adopt' | 'upgrade-arg' | 'none'

export interface TriageRow {
  analysis?: { source: string; model: string; error: string }
  version: string
  publishError?: string
  status: string
  entries: { id: string; text: string }[]
  verdicts: { entry_id: string; verdict: string; reason: string; module: string }[]
  proposals: { entry_ids: string[]; title: string; verdict: string; duplicate_of: number | null }[]
  issues: { entry_ids: string[]; number: number | null; url: string | null; comment?: boolean }[]
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
  duplicate: boolean
  published?: boolean
}

export interface TriageVersion {
  analysis?: { source: string; model: string; error: string }
  publishError?: string
  version: string
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

export function parseTriageRows(raw: unknown): { repo: string | null; rows: TriageRow[] } {
  const o = isRec(raw) ? raw : {}
  const rows = (Array.isArray(o.rows) ? o.rows : []).filter(isRec).map((r): TriageRow => {
    const v = r.verdicts
    const list = Array.isArray(v) ? v : isRec(v) && Array.isArray(v.verdicts) ? v.verdicts : []
    const props = isRec(v) && Array.isArray(v.issues) ? v.issues : []
    return {
      version: str(r.version),
      analysis: isRec(v) && isRec(v.analysis) ? { source: str(v.analysis.source), model: str(v.analysis.model), error: str(v.analysis.error) } : undefined,
      status: str(r.status),
      publishError: str(r.publish_error),
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
        comment: i.comment === true,
      })),
    }
  })
  return { repo: str(o.repo) || null, rows }
}

function parts(v: string): number[] | null {
  if (!/^\d+(\.\d+)*$/.test(v)) return null
  return v.split('.').map(Number)
}

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

function bareTitle(t: string): string {
  return t.replace(/[（(](提防|採用)[）)]\s*$/, '').trim()
}

function summarize(row: TriageRow, repo: string | null): TriageVersion {
  const text = new Map(row.entries.map((e) => [e.id, e.text]))
  const proposals = row.proposals.filter((p) => p.verdict === 'guard' || p.verdict === 'adopt')
  const covered = new Set(proposals.flatMap((p) => p.entry_ids))
  const pick = (kind: TriageVerdict): TriageItem[] =>
    row.verdicts
      .filter((v) => v.verdict === kind && !covered.has(v.entry_id))
      .map((v) => ({ id: v.entry_id, text: text.get(v.entry_id) ?? '', reason: v.reason, module: v.module }))
  const issues = proposals
    .map((p): TriageIssue => {
      const opened = row.issues.find((i) => i.entry_ids.some((id) => p.entry_ids.includes(id)))
      const number = opened?.number ?? p.duplicate_of
      const url = opened?.url ?? (number !== null && repo ? `https://github.com/${repo}/issues/${number}` : null)
      return { title: bareTitle(p.title), verdict: p.verdict as 'guard' | 'adopt', number, url, published: !!opened, duplicate: opened?.comment === true || p.duplicate_of !== null }
    })
  const state: TriageVersion['state'] =
    row.status === 'judged' || row.status === 'published'
      ? 'judged'
      : row.status === 'empty'
        ? 'empty'
        : row.status === 'failed'
          ? 'failed'
          : 'pending'
  return { version: row.version, publishError: row.publishError, analysis: row.analysis, state, guard: pick('guard'), adopt: pick('adopt'), upgrade: pick('upgrade-arg'), issues }
}

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

export function anyAnalysed(vs: TriageVersion[]): boolean {
  return vs.some((v) => v.state === 'judged' || v.state === 'empty')
}
