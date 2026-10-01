/**
 * 側欄在大量 bot 下的重繪：200 顆 bot、連續 20 次「單顆 run 狀態變化」，數每次變化重渲染幾列 BotRow、花多久。
 * 量測（50 次）：BotRow 沒有 memo 時每次變化 200 列全部重算（avg 200、max 200，約 318 ms／次）；
 * memo 化、回呼穩定之後每次只有那一列（avg 1、max 1，約 71 ms／次）。
 * 真的把 `Sidebar` 掛進 happy-dom（`react-dom/client`），用 `renderProbe` 數 `BotRow` 實際渲染次數。
 */
import test, { after } from 'node:test'
import assert from 'node:assert/strict'
import { GlobalRegistrator } from '@happy-dom/global-registrator'

GlobalRegistrator.register({ url: 'http://localhost:5173' })
// happy-dom 換掉了全域 fetch：元件掛上去時會打的 API 一律回空物件，不碰網路。
globalThis.fetch = (async () => new Response('{}', { status: 200 })) as typeof fetch
;(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true

const React = await import('react')
const { createRoot } = await import('react-dom/client')
const { useStore } = await import('../store/store.ts')
const { Sidebar } = await import('./Sidebar.tsx')

const BOTS = 200
const PROJECTS = 10
const CHANGES = 20

const counts: Record<string, number> = {}
;(globalThis as { __amRenderProbe?: (n: string) => void }).__amRenderProbe = (n) => {
  counts[n] = (counts[n] ?? 0) + 1
}

function seed() {
  const projects = Array.from({ length: PROJECTS }, (_, p) => ({ id: `p${p}`, label: `proj-${p}`, path: `/p${p}`, host: 'local' }))
  const bots = Array.from({ length: BOTS }, (_, i) => ({
    id: `b${i}`, name: `bot-${i}`, project_id: `p${i % PROJECTS}`, kind: 'claude', identity: null,
    model: null, effort: null, args: [], env: {}, autostart: false, inject_hooks: true, auto_approve: false,
    managed_by: 'user', parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
  }))
  const runs: Record<string, unknown> = {}
  for (const b of bots) {
    runs[b.id] = { id: `r-${b.id}`, bot_id: b.id, state: 'running', agent_status: 'idle', pane_id: 'w1:p1', started_at: '2026-10-01T00:00:00Z', agent_status_since: '2026-10-01T00:00:00Z' }
  }
  useStore.setState({ projects, bots, runs, botOrder: {}, projectOrder: [], botUnread: {}, connected: true, defaultConnected: true, socket: 'open' } as never)
}

test('量測：200 顆 bot、連續 20 次單顆 run 狀態變化，每次重渲染幾列 BotRow', { timeout: 300_000 }, async () => {
  const { act } = React
  seed()
  const host = document.createElement('div')
  document.body.appendChild(host)
  const root = createRoot(host)
  await act(async () => {
    root.render(React.createElement(Sidebar))
  })
  const initial = counts.BotRow ?? 0
  assert.ok(initial >= BOTS, `首次渲染要把 ${BOTS} 列都畫出來（${initial}）`)

  const perChange: number[] = []
  const t0 = performance.now()
  for (let k = 0; k < CHANGES; k++) {
    const id = `b${(k * 7) % BOTS}`
    const before = counts.BotRow ?? 0
    await act(async () => {
      useStore.setState((s) => {
        const run = s.runs[id]!
        return { runs: { ...s.runs, [id]: { ...run, agent_status: k % 2 === 0 ? 'working' : 'idle' } } } as never
      })
    })
    perChange.push((counts.BotRow ?? 0) - before)
  }
  const ms = performance.now() - t0
  const total = perChange.reduce((a, b) => a + b, 0)
  console.log(`[sidebar] bots=${BOTS} changes=${CHANGES} BotRow renders: first=${initial} total=${total} perChange avg=${(total / CHANGES).toFixed(1)} max=${Math.max(...perChange)} elapsed=${ms.toFixed(0)}ms (${(ms / CHANGES).toFixed(1)}ms/change)`)
  await act(async () => root.unmount())
  assert.ok(Math.max(...perChange) <= 5, `一顆 bot 的 run 變了，最多只該重渲染它自己那幾列（實測最多 ${Math.max(...perChange)} 列）`)
})

after(() => {
  void GlobalRegistrator.unregister()
})
