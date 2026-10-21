/**
 * mock 的 herdr integration 更新總覽（SPEC §6.9c，issue #1772）：`GET /integration-updates`、
 * `POST /hosts/{name}/herdr-integration`。只改記憶體裡的表，不碰任何設定檔。
 */
import { ApiError } from './types'

type Rec = Record<string, unknown>

const row = (name: string, state: string, installed: number | null, required: number | null) => ({
  name,
  state,
  installed_version: installed,
  required_version: required,
  path: state === 'not_installed' ? null : `/home/mock/.${name}/hooks/herdr-agent-state.sh`,
  detail: null,
})

export class MockIntegrationUpdates {
  private hosts: Rec[] = [
    {
      host: 'local',
      checked_at: '2026-10-14T12:00:00.000Z',
      error: null,
      items: [row('claude', 'outdated', 8, 11), row('codex', 'outdated', 8, 10), row('antigravity-cli', 'not_installed', null, null), row('grok', 'not_installed', null, null)],
    },
    {
      host: 'm4p',
      checked_at: '2026-10-14T12:00:00.000Z',
      error: null,
      items: [row('claude', 'current', 11, 11), row('codex', 'outdated', 9, 10), row('antigravity-cli', 'not_installed', null, null), row('grok', 'current', 3, 3)],
    },
  ]

  handle(method: string, rawPath: string, seg: string[], b: Rec): unknown {
    if (method === 'GET' && rawPath === '/integration-updates') return { hosts: this.hosts }
    if (method === 'POST' && seg[0] === 'hosts' && seg[2] === 'herdr-integration' && seg.length === 3) {
      const host = decodeURIComponent(seg[1])
      const name = String(b.name ?? '')
      const h = this.hosts.find((x) => x.host === host)
      const items = (h?.items ?? []) as Rec[]
      const item = items.find((x) => x.name === name)
      if (!h || !item) throw new ApiError(404, { error: 'not_found', what: 'host' }, 'not found')
      // 手動安裝沒裝的只限 grok／antigravity-cli（daemon 另外要求那台已登入，#1871）。
      const freshOk = item.state === 'not_installed' && (name === 'grok' || name === 'antigravity-cli')
      if (item.state !== 'outdated' && !freshOk) {
        throw new ApiError(409, { error: 'conflict', reason: 'already_current', message: `${name} 的 integration 不需要更新` }, 'conflict')
      }
      item.state = 'current'
      item.installed_version = freshOk ? 3 : item.required_version
      item.required_version = item.installed_version
      return { host, name, state: 'current', integration: item, output: `installed ${name}` }
    }
    return undefined
  }
}
