import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'

before(setupDom)
after(teardownDom)
afterEach(unmountAll)

const { HeadMemBadges } = await import('./HeadMemBadges')

const GB = 1024 ** 3
const row = (host: string, freePct: number, cpu: number) => ({
  host,
  total_bytes: 4 * GB,
  herdr_bytes: GB,
  agents_bytes: GB,
  processes: 3,
  browsers: [],
  machine: { total_bytes: 100 * GB, available_bytes: freePct * GB, cpu_pct: cpu },
  error: null,
})

let requested: string[] = []
let realFetch: typeof fetch
beforeEach(() => {
  resetStoreForTest()
  requested = []
  realFetch = globalThis.fetch
  globalThis.fetch = (async (input: string) => {
    const path = String(input).replace(/^\/api/, '')
    if (path === '/session') return new Response(JSON.stringify({ token: 't' }), { status: 200 })
    if (path.startsWith('/mem/processes')) {
      const host = new URL(path, 'http://x').searchParams.get('host') ?? ''
      requested.push(host)
      return new Response(JSON.stringify({ host, sampled_at: new Date().toISOString(), processes: [] }), { status: 200 })
    }
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch
})
afterEach(() => {
  globalThis.fetch = realFetch
})

async function flush() {
  for (let i = 0; i < 5; i++) await act(async () => new Promise<void>((r) => setImmediate(r)))
}

test('單主機：顯示單格精簡 RAM／剩餘／CPU 膠囊', async () => {
  useStore.setState({ mem: { hosts: [row('local', 55, 40)] } } as never)
  await mount(<HeadMemBadges />)
  const head = document.querySelector('.head-ram')!
  assert.ok(!head.classList.contains('multi'))
  const badges = head.querySelectorAll('.mem-badge.compact')
  assert.equal(badges.length, 1)
  assert.match(badges[0].textContent ?? '', /本機4\.0G\/55G · 40%/)
})

test('多主機：雙欄並排顯示每台主機精簡膠囊，點任一台都能在彈窗切換主機分頁', async () => {
  useStore.setState({ mem: { hosts: [row('local', 55, 40), row('m4p', 40, 9), row('r7', 48, 92)] } } as never)
  await mount(<HeadMemBadges />)
  const head = document.querySelector('.head-ram')!
  assert.ok(head.classList.contains('multi'))
  const badges = [...head.querySelectorAll('.mem-badge.compact')]
  assert.equal(badges.length, 3)
  assert.match(badges[0].textContent ?? '', /本機4\.0G\/55G · 40%/)
  assert.match(badges[1].textContent ?? '', /@m4p4\.0G\/40G · 9%/)
  assert.match(badges[2].textContent ?? '', /@r74\.0G\/48G · 92%/)
  assert.ok(badges[2].classList.contains('cpu-hot'), '告警主機直接亮紅框')

  await click(head.querySelectorAll('.mem-open')[2]!)
  await flush()
  assert.deepEqual(requested, ['r7'])
  const tabs = [...document.querySelectorAll<HTMLButtonElement>('.mem-pop-host')]
  assert.deepEqual(tabs.map((b) => b.textContent), ['本機', '@m4p', '@r7'])
  assert.equal(tabs[2].getAttribute('aria-selected'), 'true')
  await click(tabs[1])
  await flush()
  assert.deepEqual(requested, ['r7', 'm4p'])
  assert.match(document.querySelector('.mem-pop-title')?.textContent ?? '', /^m4p RAM/)
})

test('側欄頂部結構（方案 3）：第一列固定單行，多主機第二列雙欄並排', () => {
  const css = readFileSync(new URL('./sidebar.css', import.meta.url), 'utf8')
  const rule = (sel: string) => {
    const m = css.match(new RegExp(`^${sel.replace(/[.*+?^${}()|[\]\\>]/g, '\\$&')}\\s*\\{([^}]*)\\}`, 'm'))
    assert.ok(m, sel)
    return m[1]
  }
  assert.match(rule('.sidebar-head'), /flex-direction:\s*column/)
  assert.match(rule('.head-brand'), /height:\s*22px/)
  assert.match(rule('.head-ram'), /display:\s*grid/)
  assert.match(rule('.head-ram.multi'), /repeat\(2,\s*minmax\(0,\s*1fr\)\)/)
  assert.match(rule('.sidebar-head .head-ram .mem-badge'), /overflow:\s*hidden/)
})
