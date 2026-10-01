/**
 * 專案頁「其他 pane」的關閉：服務 pane（有在跑的 server）要先過確認框，shell 直接關。
 * 取消、Esc 都不送出請求；確認才送，而且帶 `confirm=true`。
 */
import test, { after, afterEach, beforeEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, keydown, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import { ProjectPanes } from './ProjectPanes'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const pane = (id: string, kind: 'service' | 'shell') => ({
  pane_id: id, host: 'local', project_id: 'p1', workspace_id: 'w1', kind, purpose: kind === 'service' ? 'dev server' : 'shell',
  owned_by: 'user', owner_bot_id: null, foreground: kind === 'service' ? 'vite' : 'zsh', listen_ports: kind === 'service' ? [5173] : [],
  last_output_at: null, read_only: false, scratch: false,
})

let requests: FakeRequest[] = []
const closes = () => requests.filter((r) => r.method === 'POST' && /\/panes\/[^/]+\/close/.test(r.path))

beforeEach(() => {
  const panes = [pane('w1:p1', 'service'), pane('w1:p2', 'shell')]
  // `refreshPanes` 讀 `GET /api/panes`（全部）與 `?unowned=1`，再依專案分組。
  requests = fakeApi((req) => (req.method === 'GET' && /\/api\/panes/.test(req.path) ? { panes: req.path.includes('unowned=1') ? [] : panes } : undefined))
  useStore.setState({ projects: [{ id: 'p1', label: 'p', path: '/p', host: 'local' }], bots: [], sidePanes: { p1: panes } } as never)
})

const closeButtonOf = (name: string) => {
  const li = [...document.querySelectorAll('.project-pane')].find((x) => x.textContent?.includes(name))!
  return [...li.querySelectorAll('button')].find((b) => b.textContent === '關閉')!
}

test('服務 pane 的「關閉」：先跳確認框，取消與 Esc 都不送請求', async () => {
  await mount(<ProjectPanes projectId="p1" workspaceId="w1" />)
  await settle()
  await click(closeButtonOf('dev server'))
  const dialog = document.querySelector('[role=alertdialog]')
  assert.ok(dialog, '服務 pane 要先確認')
  assert.match(dialog!.textContent ?? '', /vite/, '確認框要講它在跑什麼')
  assert.match(dialog!.textContent ?? '', /5173/, '也要講 listen 的 port')
  assert.equal(closes().length, 0)

  await click([...dialog!.querySelectorAll('button')].find((b) => b.textContent === '取消')!)
  assert.equal(document.querySelector('[role=alertdialog]'), null)
  assert.equal(closes().length, 0, '取消不送')

  await click(closeButtonOf('dev server'))
  await keydown(document.activeElement ?? document.body, 'Escape')
  assert.equal(document.querySelector('[role=alertdialog]'), null)
  assert.equal(closes().length, 0, 'Esc 不送')
})

test('服務 pane 確認關閉：送出 close 並帶 confirm=true', async () => {
  await mount(<ProjectPanes projectId="p1" workspaceId="w1" />)
  await settle()
  await click(closeButtonOf('dev server'))
  const dialog = document.querySelector('[role=alertdialog]')!
  await click([...dialog.querySelectorAll('button')].find((b) => b.textContent === '關閉')!)
  await settle()
  assert.equal(closes().length, 1)
  assert.match(closes()[0].path, /w1(%3A|:)p1\/close/)
  assert.match(closes()[0].path, /confirm=true/)
})

test('shell pane 的「關閉」：不用確認，直接送（不帶 confirm）', async () => {
  await mount(<ProjectPanes projectId="p1" workspaceId="w1" />)
  await settle()
  await click(closeButtonOf('shell'))
  await settle()
  assert.equal(document.querySelector('[role=alertdialog]'), null)
  assert.equal(closes().length, 1)
  assert.doesNotMatch(closes()[0].path, /confirm=true/)
})
