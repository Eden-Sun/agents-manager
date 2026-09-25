/**
 * `GET /api/changelog`：重啟套用前先看新版改了什麼（2026-09-10 使用者需求）。
 * 傳輸層錯誤也收成 `found:false` + `error`，UI 不會靜默略過。
 */
import { rawTransport } from './index'

export interface ChangelogSection {
  version: string
  body: string
}

export interface ChangelogReply {
  kind: string
  host: string
  installedVersion: string | null
  fromVersion: string | null
  found: boolean
  sections: ChangelogSection[]
  sourceUrl: string
  error: string | null
}

const CLAUDE_SOURCE = 'https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md'
const CODEX_SOURCE = 'https://github.com/openai/codex/releases'

/** codex 沒有 CHANGELOG.md，只有 releases。 */
function sourceFor(kind: string): string {
  return kind === 'codex' ? CODEX_SOURCE : CLAUDE_SOURCE
}

function isRec(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null
}

function toReply(raw: unknown, kind: string, host: string): ChangelogReply {
  const r = isRec(raw) ? raw : {}
  const sections = Array.isArray(r.sections)
    ? r.sections
        .filter(isRec)
        .map((s) => ({ version: String(s.version ?? ''), body: String(s.body ?? '') }))
        .filter((s) => s.version)
    : []
  return {
    kind: typeof r.kind === 'string' ? r.kind : kind,
    host: typeof r.host === 'string' ? r.host : host,
    installedVersion: typeof r.installed_version === 'string' ? r.installed_version : null,
    fromVersion: typeof r.from_version === 'string' ? r.from_version : null,
    found: r.found === true && sections.length > 0,
    sections,
    sourceUrl: typeof r.source_url === 'string' ? r.source_url : sourceFor(kind),
    error: typeof r.error === 'string' ? r.error : null,
  }
}

export async function fetchChangelog(
  kind: string,
  host: string,
  from: string | null,
  to: string | null = null,
): Promise<ChangelogReply> {
  const q = new URLSearchParams({ kind, host })
  if (from) q.set('from', from)
  // codex 新版還沒進磁碟，目標版本取自 pane 上的 `a -> b`。
  if (to) q.set('to', to)
  try {
    return toReply(await rawTransport.request('GET', `/changelog?${q.toString()}`), kind, host)
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    return { kind, host, installedVersion: null, fromVersion: from, found: false, sections: [], sourceUrl: sourceFor(kind), error: msg }
  }
}
