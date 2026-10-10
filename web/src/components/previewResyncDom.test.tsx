/**
 * #1211：WS 重連或 resync 後，`refreshState` 帶回的快照 status／port 變了，預覽分頁要重讀一次（GET），
 * 不然漏掉的 `preview_changed` 沒有人補，停掉的預覽會一直是 running 的 iframe。快照沒變就不多打 GET。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, setupDom, settle, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { PREVIEW_OFF, type Preview } from '../api/preview'
import type { Bot, Project } from '../api/types'
import { PreviewPanel } from './PreviewPanel'

const project = { id: 'p1', label: 'p', path: '/p', host: 'local' } as Project
const bot = { id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude', identity: null, cwd: '/p' } as unknown as Bot

const iframe = () => document.querySelector('iframe.preview-frame')

afterEach(async () => {
  await unmountAll()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

it('快照說預覽已經停了：重讀一次，iframe 收掉、換成「啟動預覽」', async () => {
  resetStoreForTest()
  let current: Preview = { ...PREVIEW_OFF, status: 'running', port: 5173, source: 'spawned' }
  const requests = fakeApi((req) => (req.method === 'GET' ? current : undefined))
  const gets = () => requests.filter((r) => r.method === 'GET').length
  useStore.setState({
    connected: true,
    projects: [project],
    bots: [{ ...bot, preview: { status: 'running', port: 5173 } } as Bot],
    previews: { b1: current },
  })
  await mount(<PreviewPanel botId="b1" />)
  await until(() => iframe() !== null, '跑著的時候畫出 iframe')
  await settle()
  const before = gets()

  // 斷線期間別的瀏覽器把預覽停了；重連後 refreshState 帶回的快照是 off。
  current = { ...PREVIEW_OFF }
  await act(async () => {
    useStore.setState({ bots: [{ ...bot, preview: { status: 'off', port: null } } as Bot] })
  })
  await until(() => gets() > before, '快照的狀態變了就重讀')
  await settle()
  // 重讀回來 `stored.status` 也變了，既有的 `seenStatus` 依賴會再讀一次；之後就收斂，不會一直讀。
  const settled = gets()
  assert.ok(settled - before <= 2, `最多兩次（快照一次、狀態變了一次），實際 +${settled - before}`)
  await settle()
  assert.equal(gets(), settled, '收斂：不會一直重讀')
  assert.equal(iframe(), null, '停掉之後 iframe 要收掉')
  assert.ok(
    [...document.querySelectorAll<HTMLButtonElement>('button')].some((b) => b.textContent === '啟動預覽'),
    '換成啟動預覽按鈕',
  )
})

it('快照沒變：不多打 GET', async () => {
  resetStoreForTest()
  const current: Preview = { ...PREVIEW_OFF, status: 'running', port: 5173, source: 'spawned' }
  const requests = fakeApi((req) => (req.method === 'GET' ? current : undefined))
  const gets = () => requests.filter((r) => r.method === 'GET').length
  useStore.setState({
    connected: true,
    projects: [project],
    bots: [{ ...bot, preview: { status: 'running', port: 5173 } } as Bot],
    previews: { b1: current },
  })
  await mount(<PreviewPanel botId="b1" />)
  await until(() => iframe() !== null, '跑著的時候畫出 iframe')
  await settle()
  const before = gets()

  // 重連後的 refreshState：陣列是新的，內容一樣。
  await act(async () => {
    useStore.setState({ bots: [{ ...bot, preview: { status: 'running', port: 5173 } } as Bot] })
  })
  await settle()
  assert.equal(gets(), before, '快照沒變就不該多打 GET')
  assert.ok(iframe() !== null, 'iframe 還在')
})
