/**
 * #1331：晶片列的三種狀態要一眼分得開：選取（藍框）、工作中（脈動點）、完成待看（黃底）。
 * DOM 部分釘住 class 與狀態的對應；CSS 部分照 `styles.css` 實際的 token 重算對比（不另寫一份色票）。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { UnreadChip } from './UnreadChip'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const project = (id: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host: 'local' })
const bot = (id: string, projectId: string) => ({
  id, name: `bot-${id}`, project_id: projectId, kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
})
const run = (botId: string, agentStatus: string) =>
  ({ id: `run-${botId}`, bot_id: botId, state: 'running', agent_status: agentStatus, workspace_id: null, pane_id: null, started_at: new Date().toISOString() }) as never

/** 依晶片上的名字找（`.unread-chip-name` 就是 bot 名）。 */
const chipNamed = (name: string) => [...document.querySelectorAll<HTMLElement>('.unread-chip')].find((e) => e.querySelector('.unread-chip-name')?.textContent === name) ?? null

beforeEach(() => {
  fakeApi()
  useStore.setState({
    projects: [project('p1')],
    bots: [bot('sel', 'p1'), bot('new', 'p1'), bot('run', 'p1')],
    runs: { run: run('run', 'working') },
    hosts: [],
    botOrder: {}, projectOrder: [], botUnread: { sel: 2, new: 1 }, hiddenBotIds: [], connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: 'sel', selectedProjectId: null, supervisorProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
})

test('選取＋完成待看：同時帶 unread 與 current（藍色數字籤由 unread 決定，藍框由 current 決定）', async () => {
  await mount(<UnreadChip />)
  await settle(50)
  const sel = chipNamed('bot-sel')!
  assert.ok(sel, '選取中的那顆要在列上')
  assert.ok(sel.classList.contains('unread') && sel.classList.contains('current'), sel.className)
  assert.equal(sel.getAttribute('aria-current'), 'true')
})

test('只有完成待看：只帶 unread，不帶 current（藍框不能跑到沒選的那顆上）', async () => {
  await mount(<UnreadChip />)
  await settle(50)
  const nu = chipNamed('bot-new')!
  assert.ok(nu, '完成待看的那顆要在列上')
  assert.ok(nu.classList.contains('unread') && !nu.classList.contains('current'), nu.className)
})

test('工作中：帶 working，不帶 unread（數字籤只給完成待看）', async () => {
  await mount(<UnreadChip />)
  await settle(50)
  const r = chipNamed('bot-run')!
  assert.ok(r, '在跑的那顆要在列上')
  assert.ok(r.classList.contains('working') && !r.classList.contains('unread'), r.className)
  assert.ok(r.querySelector('.unread-chip-dot'), '工作中是脈動點，不是數字籤')
})

// ---- CSS：照 styles.css 的 token 重算，不另外寫色票 ----

const dir = new URL('.', import.meta.url).pathname
const styles = readFileSync(join(dir, '../styles.css'), 'utf8')
const chipCss = readFileSync(join(dir, 'unreadChip.css'), 'utf8')

/** 取一個區塊的內文：`header` 是區塊開頭那段文字（含 `{`），回傳到對應的 `}` 為止。 */
function blockOf(src: string, header: string): string {
  const at = src.indexOf(header)
  assert.ok(at >= 0, `找不到 ${header}`)
  const open = src.indexOf('{', at)
  let depth = 0
  for (let i = open; i < src.length; i++) {
    if (src[i] === '{') depth++
    else if (src[i] === '}' && --depth === 0) return src.slice(open + 1, i)
  }
  throw new Error(`區塊沒閉合：${header}`)
}
const hexOf = (body: string, name: string) => {
  const m = body.match(new RegExp(`--${name}:\\s*(#[0-9a-fA-F]{6})\\b`))
  assert.ok(m, `缺 --${name}`)
  return m[1]
}
const rgb = (h: string) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16) / 255)
const lin = (c: number) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4)
const lum = (h: string) => {
  const [r, g, b] = rgb(h).map(lin)
  return 0.2126 * r + 0.7152 * g + 0.0722 * b
}
const contrast = (a: string, b: string) => {
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x)
  return (hi + 0.05) / (lo + 0.05)
}
const THEMES = [
  { name: '淺色', header: ':root {' },
  { name: '深色（跟系統）', header: ':root:not([data-theme="light"]) {' },
  { name: '深色（手動）', header: ':root[data-theme="dark"] {' },
]

/** 去掉註解後，依 `選擇器 { 宣告 }` 切出每一條規則（選擇器可以是逗號清單）。 */
function rulesOf(src: string): { selectors: string[]; body: string }[] {
  const bare = src.replace(/\/\*[\s\S]*?\*\//g, '')
  return [...bare.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((m) => ({ selectors: m[1].split(',').map((s) => s.trim()), body: m[2] }))
}
const keepWarmCss = readFileSync(join(dir, 'keepWarmChip.css'), 'utf8')
const cacheCss = readFileSync(join(dir, 'cacheClock.css'), 'utf8')
const UNREAD_WARM = '.unread-chip.unread .unread-chip-warm'

// 2026-10-12 使用者：「已完成對話只要數字藍色就好，不要用 bg，會蓋到快取的時間倒數」。
test('完成待看不塗底：任何帶 .unread 的晶片規則都不設 background，底色留給快取倒數', () => {
  for (const r of rulesOf(chipCss)) {
    for (const sel of r.selectors) {
      if (!/\.unread-chip[^ ]*\.unread(?![-\w])[^ ]*$/.test(sel)) continue
      assert.doesNotMatch(r.body, /background/, `${sel} 不可設底色`)
    }
  }
  assert.doesNotMatch(chipCss.replace(/\/\*[\s\S]*?\*\//g, ''), /--done-fill/)
})

test('完成待看的數字籤是藍的：基底用 accent，沒有 .unread 的覆寫把它換掉', () => {
  const base = blockOf(chipCss, '\n.unread-chip-n {')
  assert.match(base, /background:\s*var\(--accent\)/)
  assert.match(base, /color:\s*var\(--accent-text\)/)
  assert.ok(!rulesOf(chipCss).some((r) => r.selectors.some((x) => /\.unread .*\.unread-chip-n$/.test(x))), '不可再覆寫未讀晶片的數字籤')
})

test('快取倒數在完成待看的晶片上照一般濃度畫（不再因為實心底而調淡）', () => {
  for (const r of rulesOf(cacheCss)) {
    if (!/--cache-(strong|faint)-pct:/.test(r.body)) continue
    assert.ok(!r.selectors.some((x) => /\.unread(?![-\w])/.test(x)), r.selectors.join(', '))
  }
})

for (const theme of THEMES) {
  test(`${theme.name}：選取藍框、藍色數字籤的對比符合目標（框對面板 ≥ 3、籤上的字 ≥ 4.5）`, () => {
    const body = blockOf(styles, theme.header)
    const accent = hexOf(body, 'accent')
    assert.ok(contrast(accent, hexOf(body, 'bg-panel')) >= 3, `${theme.name}：藍框對面板 ${contrast(accent, hexOf(body, 'bg-panel')).toFixed(2)}:1`)
    assert.ok(contrast(accent, hexOf(body, 'accent-text')) >= 4.5, `${theme.name}：數字籤字對藍底 ${contrast(accent, hexOf(body, 'accent-text')).toFixed(2)}:1`)
  })
}

test('keepWarmChip.css 不把未讀晶片上的保溫記號設成白色（未讀沒有實心底）', () => {
  for (const r of rulesOf(keepWarmCss)) {
    if (!r.selectors.includes(UNREAD_WARM)) continue
    assert.doesNotMatch(r.body, /color:\s*(#fff\b|#ffffff\b|white\b)/i, `${UNREAD_WARM} 不可反白`)
  }
})
