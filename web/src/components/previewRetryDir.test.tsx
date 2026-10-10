/**
 * #1103：預覽啟動失敗後按「重試」要重試失敗的那個目錄（帶 `dir`），不是不帶參數讓 daemon 挑第一個候選、或接上別的 dev server。
 * 失敗的目錄不在候選清單裡（例如 bot 工作目錄）時不帶，免得 daemon 以 400「不是候選目錄」拒絕。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { PREVIEW_OFF, type Preview } from '../api/preview'
import type { Bot, Project } from '../api/types'
import { PreviewPanel } from './PreviewPanel'

const project = { id: 'p1', label: 'p', path: '/p', host: 'local' } as Project
const bot = { id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude', identity: null, cwd: '/p' } as unknown as Bot

const failed = (over: Partial<Preview> = {}): Preview => ({
  ...PREVIEW_OFF,
  status: 'failed',
  dir: '/p/apps/b',
  error: 'missing package',
  candidates: [
    { dir: '/p/apps/a', command: 'npm run dev' },
    { dir: '/p/apps/b', command: 'npm run dev' },
  ],
  ...over,
})

const retryButton = () => [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent === '重試') ?? null

/** 失敗畫面按「重試」，回傳這一次送出的 POST（路徑與 body）。 */
async function retry(preview: Preview) {
  const requests = fakeApi((req) => (req.method === 'GET' || req.method === 'POST' ? preview : undefined))
  useStore.setState({ connected: true, bots: [bot], projects: [project], previews: { b1: preview } })
  await mount(<PreviewPanel botId="b1" />)
  await until(() => retryButton() !== null, '失敗畫面的重試鈕')
  await click(retryButton()!)
  await until(() => requests.some((r) => r.method === 'POST'), '重試送出 POST')
  return requests.filter((r) => r.method === 'POST')
}

afterEach(async () => {
  await unmountAll()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

it('多個候選目錄時，重試帶上失敗的那個目錄（apps/b），不讓 daemon 改起第一個候選', async () => {
  resetStoreForTest()
  const [post] = await retry(failed())
  assert.ok(post.path.endsWith('/bots/b1/preview'), post.path)
  assert.deepEqual(post.body, { dir: '/p/apps/b' }, `重試要重試 apps/b，實際 body ${JSON.stringify(post.body)}`)
})

it('失敗的目錄不在候選清單裡時不帶 dir（避免 daemon 以 400 拒絕），照舊由 daemon 決定', async () => {
  resetStoreForTest()
  const [post] = await retry(failed({ dir: '/p/bot-cwd', candidates: [{ dir: '/p/apps/a', command: 'npm run dev' }, { dir: '/p/apps/b', command: 'npm run dev' }] }))
  assert.equal(post.body, undefined, `不是候選就不帶，實際 body ${JSON.stringify(post.body)}`)
})
