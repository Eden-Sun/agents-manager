/**
 * 瀏覽器儲存受阻（無痕擋寫、配額滿、讀取被封鎖）時，預覽開關與寬度在這一頁仍要生效（issue #1809）；
 * 儲存正常時照舊寫入，別分頁的 storage 事件仍同步。正式 hook 掛進 happy-dom 驗。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act } from 'react'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import {
  resetPreviewLayoutForTest,
  setPreviewColOpen,
  setPreviewColWidth,
  usePreviewColOpen,
  usePreviewColWidth,
} from './previewLayout'
import { resetMobilePreviewForTest, setMobilePreviewOpen, useMobilePreviewOpen } from './mobilePreview'

const OPEN_KEY = 'am.previewCol.openBots'
const WIDTH_KEY = 'am.previewCol.width'

type Mode = 'ok' | 'denied' | 'quota' | 'readDenied'

/** 假的 localStorage：`mode` 決定寫入或讀取要丟例外；`data` 是「磁碟」內容，也拿來模擬別的分頁寫入。 */
function installStorage(initial: Record<string, string> = {}) {
  const data = new Map(Object.entries(initial))
  const state = { mode: 'ok' as Mode }
  const fake = {
    getItem(key: string) {
      if (state.mode === 'readDenied') throw new DOMException('denied', 'SecurityError')
      return data.get(key) ?? null
    },
    setItem(key: string, value: string) {
      if (state.mode === 'denied') throw new DOMException('denied', 'SecurityError')
      if (state.mode === 'quota') throw new DOMException('full', 'QuotaExceededError')
      data.set(key, String(value))
    },
    removeItem(key: string) {
      data.delete(key)
    },
    clear() {
      data.clear()
    },
    key(i: number) {
      return [...data.keys()][i] ?? null
    },
    get length() {
      return data.size
    },
  }
  Object.defineProperty(window, 'localStorage', { configurable: true, value: fake })
  // 記憶體快照是模組層的：前面的測試檔留下的值不能帶進來。
  resetPreviewLayoutForTest()
  resetMobilePreviewForTest()
  return { data, state }
}

function Probe({ bot }: { bot: string }) {
  const open = usePreviewColOpen(bot)
  const width = usePreviewColWidth()
  const mobile = useMobilePreviewOpen()
  return <p data-testid="probe">{`${open}|${width}|${mobile}`}</p>
}

const probe = () => document.querySelector('[data-testid="probe"]')!.textContent

/** 讓寫入事件跑在 act 裡，React 的重繪照真的流程跑完才回來。 */
const update = (fn: () => void) =>
  act(async () => {
    fn()
  })

let originalStorage: unknown

before(async () => {
  await setupDom()
  originalStorage = window.localStorage
})
afterEach(async () => {
  await unmountAll()
  Object.defineProperty(window, 'localStorage', { configurable: true, value: originalStorage })
  resetPreviewLayoutForTest()
  resetMobilePreviewForTest()
})
after(async () => {
  await teardownDom()
})

for (const [label, mode] of [
  ['寫入被擋（SecurityError）', 'denied'],
  ['配額滿（QuotaExceededError）', 'quota'],
] as const) {
  test(`${label}：開關、寬度與手機預覽照樣生效`, async () => {
    const storage = installStorage()
    storage.state.mode = mode
    await mount(<Probe bot="b1" />)
    assert.equal(probe(), 'false|null|false')

    await update(() => setPreviewColOpen('b1', true))
    assert.equal(probe(), 'true|null|false')

    await update(() => setPreviewColWidth(500))
    assert.equal(probe(), 'true|500|false')

    await update(() => setMobilePreviewOpen(true))
    assert.equal(probe(), 'true|500|true')

    await update(() => setPreviewColOpen('b1', false))
    await update(() => setMobilePreviewOpen(false))
    assert.equal(probe(), 'false|500|false')
    assert.equal(storage.data.size, 0, '寫不進去的值不該出現在儲存裡')
  })
}

test('寫入受阻時，每顆 bot 的開關互不影響', async () => {
  const storage = installStorage()
  storage.state.mode = 'denied'
  await mount(
    <>
      <Probe bot="b1" />
      <Probe bot="b2" />
    </>,
  )
  await update(() => setPreviewColOpen('b1', true))
  const [first, second] = [...document.querySelectorAll('[data-testid="probe"]')].map((el) => el.textContent)
  assert.equal(first, 'true|null|false')
  assert.equal(second, 'false|null|false')
})

test('讀取被擋（getItem 丟例外）：不崩、預設收合，之後寫入仍生效', async () => {
  const storage = installStorage({ [OPEN_KEY]: '{"b1":true}', [WIDTH_KEY]: '500' })
  storage.state.mode = 'readDenied'
  await mount(<Probe bot="b1" />)
  assert.equal(probe(), 'false|null|false')

  await update(() => setPreviewColOpen('b1', true))
  assert.equal(probe(), 'true|null|false')
})

test('儲存正常時照舊寫入 localStorage', async () => {
  const storage = installStorage()
  await mount(<Probe bot="b1" />)
  await update(() => setPreviewColOpen('b1', true))
  await update(() => setPreviewColWidth(500))
  await update(() => setMobilePreviewOpen(true))
  assert.equal(probe(), 'true|500|true')
  assert.equal(storage.data.get(OPEN_KEY), '{"b1":true}')
  assert.equal(storage.data.get(WIDTH_KEY), '500')
  assert.equal(storage.data.get('am.mobilePreview.open'), '1')
})

test('別分頁的 storage 事件會同步進來（跨分頁同步保留）', async () => {
  const storage = installStorage()
  await mount(<Probe bot="b1" />)
  assert.equal(probe(), 'false|null|false')

  // 另一個分頁寫入後送出 storage 事件（同一個視窗裡直接寫不會送事件）。
  storage.data.set(OPEN_KEY, '{"b1":true}')
  storage.data.set(WIDTH_KEY, '600')
  storage.data.set('am.mobilePreview.open', '1')
  await update(() => window.dispatchEvent(new Event('storage')))
  assert.equal(probe(), 'true|600|true')
})
