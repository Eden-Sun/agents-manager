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

test('選取＋完成待看：同時帶 unread 與 current（黃底由 unread 決定，藍框由 current 決定）', async () => {
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

test('工作中：帶 working，不帶 unread（黃底只給完成待看）', async () => {
  await mount(<UnreadChip />)
  await settle(50)
  const r = chipNamed('bot-run')!
  assert.ok(r, '在跑的那顆要在列上')
  assert.ok(r.classList.contains('working') && !r.classList.contains('unread'), r.className)
  assert.ok(r.querySelector('.unread-chip-dot'), '工作中是脈動點，不是黃底')
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
/** `color-mix(in srgb, A p%, B)` 在 sRGB 上是逐分量線性內插，跟瀏覽器算的一樣。 */
const mixHex = (a: string, pct: number, b: string) => {
  const [ra, ga, ba] = rgb(a)
  const [rb, gb, bb] = rgb(b)
  const p = pct / 100
  return '#' + [ra * p + rb * (1 - p), ga * p + gb * (1 - p), ba * p + bb * (1 - p)].map((c) => Math.round(c * 255).toString(16).padStart(2, '0')).join('')
}

const THEMES = [
  { name: '淺色', header: ':root {' },
  { name: '深色（跟系統）', header: ':root:not([data-theme="light"]) {' },
  { name: '深色（手動）', header: ':root[data-theme="dark"] {' },
]

for (const theme of THEMES) {
  test(`${theme.name}：完成待看的字、選取藍框的對比符合目標（字 ≥ 4.5、框 ≥ 3）`, () => {
    const body = blockOf(styles, theme.header)
    const doneDecl = body.match(/--done-fill:\s*color-mix\(in srgb,\s*var\(--lamp-yellow\)\s*(\d+)%,\s*var\(--bg-panel\)\)/)
    assert.ok(doneDecl, `${theme.name} 的 --done-fill 要是「黃混面板」的 color-mix`)
    const pct = Number(doneDecl[1])
    const fill = mixHex(hexOf(body, 'lamp-yellow'), pct, hexOf(body, 'bg-panel'))
    const ink = hexOf(body, 'text')
    assert.match(body, /--done-ink:\s*var\(--text\)/, '完成待看的字用 --text')
    const textRatio = contrast(fill, ink)
    const ringRatio = contrast(hexOf(body, 'accent'), fill)
    assert.ok(textRatio >= 4.5, `${theme.name}：完成待看字對底 ${textRatio.toFixed(2)}:1（底 ${fill}）`)
    assert.ok(ringRatio >= 3, `${theme.name}：選取藍框對黃底 ${ringRatio.toFixed(2)}:1`)
    // hover 比照：底往面板色靠 20%，字不能掉下去。
    const hoverFill = mixHex(fill, 80, hexOf(body, 'bg-panel'))
    assert.ok(contrast(hoverFill, ink) >= 4.5, `${theme.name}：hover 字對底 ${contrast(hoverFill, ink).toFixed(2)}:1`)
  })
}

test('未讀晶片的底改用 --done-fill，不再是 accent；選取＋未讀也是黃底、藍框照留', () => {
  const unread = blockOf(chipCss, '\n.unread-chip.unread {')
  assert.match(unread, /background:\s*var\(--done-fill\)/)
  assert.doesNotMatch(unread, /background:\s*var\(--accent\)/)
  const both = blockOf(chipCss, '\n.unread-chip.current.unread {')
  assert.match(both, /background:\s*var\(--done-fill\)/)
  assert.match(both, /border-color:\s*var\(--accent\)/, '選取的藍框要留著')
  const count = blockOf(chipCss, '\n.unread-chip.unread .unread-chip-n {')
  assert.doesNotMatch(count, /var\(--accent\)/, '黃底上的數字籤不能再是藍的')
})

// #1402：未讀（黃底）晶片上的保溫記號 ♨。`keepWarmChip.css` 的反白規則是 #1331 之前「未讀＝實心藍底」留下的，要移除。
const keepWarmCss = readFileSync(join(dir, 'keepWarmChip.css'), 'utf8')
/** 去掉註解後，依 `選擇器 { 宣告 }` 切出每一條規則（選擇器可以是逗號清單）。 */
function rulesOf(src: string): { selectors: string[]; body: string }[] {
  const bare = src.replace(/\/\*[\s\S]*?\*\//g, '')
  return [...bare.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((m) => ({ selectors: m[1].split(',').map((s) => s.trim()), body: m[2] }))
}
const UNREAD_WARM = '.unread-chip.unread .unread-chip-warm'
/** `#abc`／`#aabbcc`／`white` 都換成 `#rrggbb`，好跟 `contrast` 用。 */
function toHex6(c: string): string {
  const v = c.toLowerCase()
  if (v === 'white') return '#ffffff'
  if (v.length === 4) return '#' + [1, 2, 3].map((i) => v[i] + v[i]).join('')
  return v
}

for (const theme of THEMES) {
  test(`${theme.name}：未讀晶片上的保溫記號（實際套用的顏色）對黃底至少 3:1`, () => {
    const body = blockOf(styles, theme.header)
    const doneDecl = body.match(/--done-fill:\s*color-mix\(in srgb,\s*var\(--lamp-yellow\)\s*(\d+)%,\s*var\(--bg-panel\)\)/)
    assert.ok(doneDecl, `${theme.name} 的 --done-fill 要是「黃混面板」的 color-mix`)
    const fill = mixHex(hexOf(body, 'lamp-yellow'), Number(doneDecl[1]), hexOf(body, 'bg-panel'))
    // 套用的顏色：這條選擇器有自己的 color 就用它，沒有才是 --keep-warm。
    const override = rulesOf(keepWarmCss).find((r) => r.selectors.includes(UNREAD_WARM))?.body.match(/color:\s*(#[0-9a-fA-F]{3}(?:[0-9a-fA-F]{3})?\b|white\b)/i)?.[1]
    const ink = override ? toHex6(override) : hexOf(blockOf(keepWarmCss, theme.header), 'keep-warm')
    const ratio = contrast(ink, fill)
    assert.ok(ratio >= 3, `${theme.name}：未讀晶片的保溫記號 ${ink} 對黃底 ${fill} 只有 ${ratio.toFixed(2)}:1`)
  })
}

test('keepWarmChip.css 不再把未讀晶片上的保溫記號設成白色（那是未讀＝藍底時代的反白）', () => {
  for (const r of rulesOf(keepWarmCss)) {
    if (!r.selectors.includes(UNREAD_WARM)) continue
    assert.doesNotMatch(r.body, /color:\s*(#fff\b|#ffffff\b|white\b)/i, `${UNREAD_WARM} 不可反白`)
  }
})
