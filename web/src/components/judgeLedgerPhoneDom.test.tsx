/**
 * Jev 帳本在手機（issue #1466）：表格外層一定有自己的橫向捲動容器（整頁不橫捲），每格帶 `data-label`
 * 讓 ≤640px 的 CSS 把一列攤成「標籤：值」的卡片，數字欄標 `num` 不折行。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, settle, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest } from '../store/store'
import { JudgeLedger } from './JudgeLedger'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

const ago = (h: number) => new Date(Date.now() - h * 3600_000).toISOString()

test('帳本兩張表都在捲動容器裡，每個 td 有 data-label，數字欄標 num', async () => {
  globalThis.fetch = (async (input: string) => {
    const path = String(input).replace(/^\/api/, '').split('?')[0]
    if (path === '/session') return new Response(JSON.stringify({ token: 't' }), { status: 200 })
    const rows = [
      { id: 'r1', at: ago(1), regex_verdict: 'stuck_queued', bot_id: null, jev_is_live_ui: 0.9, ms: 620, error: null },
    ]
    return new Response(JSON.stringify({ enabled: false, projects: [], model: 'jev', rows }), { status: 200 })
  }) as unknown as typeof fetch
  const root = await mount(<JudgeLedger />)
  await settle(200)
  const tables = [...root.querySelectorAll<HTMLTableElement>('table.judge-ledger-table')]
  assert.equal(tables.length, 2)
  for (const t of tables) {
    assert.ok(t.parentElement?.classList.contains('judge-ledger-scroll'), '表格外層要有捲動容器')
    const heads = [...t.querySelectorAll('thead th')].map((th) => th.textContent)
    for (const tr of t.querySelectorAll('tbody tr')) {
      const cells = [...tr.querySelectorAll('td')]
      assert.equal(cells.length, heads.length)
      cells.forEach((td, i) => assert.equal(td.getAttribute('data-label'), heads[i], `第 ${i} 格的標籤要等於表頭`))
    }
  }
  // 最近 20 筆：時間、機率、耗時是數字欄。
  const recent = tables[1]!.querySelector('tbody tr')!
  for (const label of ['時間', '機率', '耗時 ms']) {
    assert.ok(recent.querySelector(`td[data-label="${label}"]`)?.classList.contains('num'), `${label} 要標 num`)
  }
})
