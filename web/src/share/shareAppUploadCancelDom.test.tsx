/**
 * #1661：分享頁移除正在上傳的附件，要中止它、釋放上傳的序列名額，下一張才能開始；
 * 舊的上傳晚回不能清掉新工作的名額；卸載時還在傳的要中止。
 * 這裡的 client.upload 故意不看 signal（由測試決定何時回），所以名額釋放不能靠底層真的中止。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { flush } from '../testing/flush'
import { skipTimerDelays } from '../testing/skipTimerDelays'
import { ShareApp } from './ShareApp'
import type { ShareClient } from './shareApi'
import { mockShareClient } from './shareMock'

afterEach(unmountAll)
// share mock 每個請求固定睡 120ms（`guard()`）：那是假的網路延遲，不是要測的行為，略過它。
let fast: ReturnType<typeof skipTimerDelays>
before(() => {
  setupDom()
  fast = skipTimerDelays([120])
})
after(async () => {
  fast.restore()
  await teardownDom()
})

const TOKEN = 'demo_share_token_0123456789'

interface Call {
  name: string
  signal?: AbortSignal
  resolve: () => void
  reject: (e: Error) => void
}

/** 每次 upload 都停在這裡，等測試決定結果；收下的 signal 記在 `calls` 裡。 */
function gatedUploads(): { client: ShareClient; calls: Call[] } {
  const calls: Call[] = []
  const client = mockShareClient(TOKEN)
  client.upload = (file: File, signal?: AbortSignal) =>
    new Promise((resolve, reject) => {
      calls.push({ name: file.name, signal, resolve: () => resolve({ id: `att-${file.name}`, name: file.name }), reject, })
    })
  return { client, calls }
}

/** 掛上 ShareApp 並等到載入完（不再是「載入中…」）。 */
async function mountReady(client: ShareClient): Promise<void> {
  await mount(<ShareApp client={client} />)
  await until(() => !document.body.textContent?.includes('載入中…'), 'ShareApp 載入完')
}

async function pick(...names: string[]): Promise<void> {
  const input = document.querySelector('input[type=file]') as HTMLInputElement
  await act(async () => {
    Object.defineProperty(input, 'files', { configurable: true, value: names.map((n) => new File([n], n, { type: 'text/plain' })) })
    input.dispatchEvent(new Event('change', { bubbles: true }))
  })
}

const row = (name: string): HTMLElement | null => [...document.querySelectorAll<HTMLElement>('.sh-pending li')].find((li) => li.textContent?.includes(name)) ?? null
const removeBtn = (name: string) => document.querySelector(`button[aria-label="移除 ${name}"]`) as HTMLButtonElement

test('移除正在上傳的附件：中止它的上傳，下一張馬上開始', async () => {
  const { client, calls } = gatedUploads()
  await mountReady(client)
  await pick('A.txt', 'B.txt')
  await until(() => calls.length === 1, 'A 開始上傳')
  assert.match(row('B.txt')!.textContent!, /等待中/)

  await click(removeBtn('A.txt'))
  await until(() => calls.length === 2, '移除 A 之後 B 要開始')
  assert.equal(calls[0].signal?.aborted, true, '正在傳的 A 要中止')
  assert.equal(calls[1].name, 'B.txt')
  assert.equal(row('A.txt'), null)
  assert.match(row('B.txt')!.textContent!, /上傳中/)

  await act(async () => calls[1].resolve())
  await until(() => row('B.txt')?.classList.contains('ok') ?? false, 'B 傳好')
  assert.equal(calls.length, 2)
})

test('移除的舊上傳晚回（失敗）不清掉下一張的名額：B 還在傳時 C 不能插進來', async () => {
  const { client, calls } = gatedUploads()
  await mountReady(client)
  await pick('A.txt', 'B.txt', 'C.txt')
  await until(() => calls.length === 1, 'A 開始上傳')

  await click(removeBtn('A.txt'))
  await until(() => calls.length === 2, 'B 開始上傳')
  await act(async () => calls[0].reject(new Error('late')))
  await flush()
  assert.equal(calls.length, 2, 'B 還在傳，C 不能開始')
  assert.match(row('B.txt')!.textContent!, /上傳中/)
  assert.equal(row('A.txt'), null, '晚回的失敗不回到已移除的項目')

  await act(async () => calls[1].resolve())
  await until(() => calls.length === 3, 'B 傳好之後 C 開始')
  assert.equal(calls[2].name, 'C.txt')
})

test('移除等待中的附件不發請求；移除已傳好的附件不影響正在傳的', async () => {
  const { client, calls } = gatedUploads()
  await mountReady(client)
  await pick('A.txt', 'B.txt', 'C.txt')
  await until(() => calls.length === 1, 'A 開始上傳')

  await click(removeBtn('B.txt'))
  await act(async () => calls[0].resolve())
  await until(() => calls.length === 2, 'A 傳好之後 C 開始')
  assert.equal(calls[1].name, 'C.txt', '被移除的 B 不發請求')
  assert.equal(row('B.txt'), null)

  await click(removeBtn('A.txt'))
  await flush()
  assert.equal(calls[1].signal?.aborted, false, '移除已傳好的 A 不中止正在傳的 C')
  assert.equal(calls.length, 2)
  await act(async () => calls[1].resolve())
  await until(() => row('C.txt')?.classList.contains('ok') ?? false, 'C 傳好')
})

test('卸載時中止還在傳的附件', async () => {
  const { client, calls } = gatedUploads()
  await mountReady(client)
  await pick('A.txt')
  await until(() => calls.length === 1, 'A 開始上傳')
  await unmountAll()
  assert.equal(calls[0].signal?.aborted, true, '離開頁面後不再讓上傳跑下去')
})
