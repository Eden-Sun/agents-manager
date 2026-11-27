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

const capsule = () => document.querySelector<HTMLElement>('[data-k="head-hosts"]')

test('單主機：第二列直接放 RAM 膠囊，不出現多主機摘要', async () => {
  useStore.setState({ mem: { hosts: [row('local', 55, 40)] } } as never)
  await mount(<HeadMemBadges pane={<span className="pane-badge">pane 3</span>} />)
  const head = document.querySelector('.head-ram')!
  assert.ok(!head.classList.contains('multi'))
  assert.equal(capsule(), null)
  assert.equal(head.querySelectorAll('.mem-badge').length, 1)
  assert.equal(head.firstElementChild?.className, 'pane-badge', 'pane 在第二列最前面')
})

test('多主機無告警：收成一顆本機摘要＋「+N」，tooltip 每台一行', async () => {
  useStore.setState({ mem: { hosts: [row('local', 55, 40), row('m4p', 40, 9), row('r7', 50, 30)] } } as never)
  await mount(<HeadMemBadges />)
  const c = capsule()!
  assert.ok(!c.classList.contains('alert'))
  assert.match(c.textContent ?? '', /本機4\.0G·40%\+2▾/)
  const lines = c.title.split('\n').filter((l) => l.includes('RAM'))
  assert.equal(lines.length, 3)
  assert.match(lines[1], /^@m4p：RAM 4\.0G · 剩 40G（40%） · CPU 9%$/)
  assert.equal(document.querySelectorAll('.head-ram .mem-badge').length, 1, '不再一台一列')
})

test('多主機有告警：優先顯示告警主機、外框亮紅；點開可切換各主機的行程樹', async () => {
  useStore.setState({ mem: { hosts: [row('local', 55, 40), row('m4p', 40, 9), row('r7', 48, 92)] } } as never)
  await mount(<HeadMemBadges />)
  const c = capsule()!
  assert.ok(c.classList.contains('alert'))
  assert.match(c.textContent ?? '', /主機 3·@r7 92%▾/)
  await click(document.querySelector('.head-ram .mem-open')!)
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

test('側欄頂部固定兩列：不換行、摘要膠囊吃剩餘寬度且可截斷', () => {
  const css = readFileSync(new URL('./sidebar.css', import.meta.url), 'utf8')
  const rule = (sel: string) => {
    const m = css.match(new RegExp(`^${sel.replace(/[.*+?^${}()|[\]\\>]/g, '\\$&')}\\s*\\{([^}]*)\\}`, 'm'))
    assert.ok(m, sel)
    return m[1]
  }
  assert.match(rule('.sidebar-head'), /flex-direction:\s*column/)
  assert.match(rule('.head-brand'), /height:\s*22px/)
  assert.match(rule('.head-ram'), /flex-wrap:\s*nowrap/)
  assert.match(rule('.head-ram'), /height:\s*20px/)
  assert.match(rule('.head-ram > .mem-wrap'), /flex:\s*1 1 auto;[\s\S]*min-width:\s*0/)
  assert.match(rule('.sidebar-head .head-ram .mem-badge'), /overflow:\s*hidden/)
  assert.doesNotMatch(css, /\.head-mem-row|\.head-right/)
})
