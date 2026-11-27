import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { Sidebar } from './Sidebar'

afterEach(async () => {
  localStorage.clear()
  resetStoreForTest()
  await unmountAll()
})
before(setupDom)
after(teardownDom)

beforeEach(() => {
  fakeApi()
  resetStoreForTest()
})

const css = (rel: string) => readFileSync(new URL(rel, import.meta.url), 'utf8')

function block(text: string, header: string): string {
  const start = text.lastIndexOf(header)
  assert.ok(start >= 0, `missing ${header}`)
  let depth = 0
  for (let i = text.indexOf('{', start); i < text.length; i++) {
    if (text[i] === '{') depth++
    else if (text[i] === '}' && --depth === 0) return text.slice(start, i + 1)
  }
  throw new Error('unbalanced')
}

test('環境設定副標拆成不可斷開的片段，窄側欄時整段換行而非在字中間折斷', async () => {
  useStore.setState({
    projects: [], bots: [], runs: {}, botOrder: {}, projectOrder: [], botUnread: {},
    connected: true, defaultConnected: true, socket: 'open', selectedBotId: null, selectedProjectId: null,
    sidePanes: {}, unownedPanes: [],
    hosts: [{ name: 'm4p', connected: true }, { name: 'nas', connected: false }],
  } as never)
  await mount(<Sidebar />)
  await settle()

  const note = document.querySelector('.disclosure-note.env-note')
  assert.ok(note)
  const parts = [...note.querySelectorAll('.env-note-part')].map((p) => p.textContent)
  assert.equal(parts.length, 3)
  assert.equal(parts[0], '本機 + 2')
  assert.equal(parts[1], '・ 1 未連線')
  assert.match(parts[2] ?? '', /^・ 身份 \d+$/)

  const side = css('./sidebar.css')
  assert.match(block(side, '.disclosure-note.env-note {'), /flex-wrap: wrap;[\s\S]*justify-content: flex-end;/)
  assert.match(block(side, '.env-note-part {'), /white-space: nowrap;/)
})

test('側欄頂部出現部署狀態時，徽章橫排留在第一列，不把 RAM 或主題鈕擠到別列', () => {
  const side = css('./sidebar.css')
  assert.match(block(side, '.sidebar-head .head-brand-title > .head-deploy {'), /flex-direction: row;[\s\S]*overflow: hidden;/)
  assert.match(block(side, '.head-brand-title {'), /min-width: 0;[\s\S]*white-space: nowrap;/)
  assert.match(block(side, '.head-brand-meta {'), /flex: none;/)
})

test('平板直向 bot header 標題收斂寬度，把空間讓給額度條', () => {
  const head = css('./desktopBotHead.css')
  const media = block(head, '@media (width >= 641px) and (width <= 1024px) {')
  assert.match(media, /\.bot-head \.main-title \{[^}]*flex: 0 1 auto;[^}]*max-width: 14em;/)
})

test('平板右側有 Preview 直排頁籤時，Toast 往左讓開不蓋住頁籤', () => {
  const styles = css('../styles.css')
  const media = block(styles, '.app:has(> .preview-col.rail) > .notices {')
  assert.match(media, /right: calc\(max\(12px, env\(safe-area-inset-right\)\) \+ 40px\);/)
  assert.match(styles, /\.app:has\(> \.preview-col\.rail\):has\(> \.shelf:not\(\.open\)\) > \.notices \{\s*right: calc\(max\(12px, env\(safe-area-inset-right\)\) \+ 80px\);/)
})
