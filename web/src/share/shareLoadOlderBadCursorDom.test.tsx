/**
 * 載入較早訊息時游標那一則已經不在對話裡（daemon 回 400 `bad_cursor`，API.md §5.7）：
 * 照 API.md 的正解重載第一頁，不拿同一個游標反覆重試，也不顯示「連不上」。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import type { ShareClient } from './shareApi'
import { ShareHttpError, type ShareMessage } from './shareModel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const msg = (id: string, role: 'user' | 'assistant', text: string, minute: number): ShareMessage => ({
  id,
  role,
  text,
  created_at: new Date(Date.UTC(2024, 0, 1, 0, minute)).toISOString(),
  attachments: [],
})

test('載入較早訊息遇到 400 bad_cursor：重載第一頁，不顯示「連不上」，也不卡在重試', async () => {
  const calls: Array<string | undefined> = []
  const recent = [msg('new-0', 'user', '近文 0', 0), msg('new-1', 'assistant', '近文 1', 1)]
  const client: ShareClient = {
    async messages(before) {
      calls.push(before)
      // 游標那一則之後被撤回／刪掉：daemon 拒收這個游標。
      if (before) throw new ShareHttpError(400, null, 'bad_cursor')
      return { bot_name: 'b', status: 'idle', messages: recent, has_more: true }
    },
    async send() {},
    async upload() {
      return { id: 'a', name: 'a' }
    },
    async files() {
      return []
    },
    fileUrl: () => '/s/t/api/files/a',
    previewUrl: () => '/s/t/api/files/a?inline=1',
    fileBlob: async () => new Blob([]),
    subscribe: () => () => {},
  }
  await mount(<ShareApp client={client} />)
  await settle(50)
  const btn = [...document.querySelectorAll('button')].find((b) => /較早/.test(b.textContent ?? ''))
  assert.ok(btn, '有較早訊息時要能載入')
  await click(btn!)
  await settle(80)
  assert.equal(calls.at(-1), undefined, '游標失效後重載第一頁（不帶 before）')
  assert.equal(document.querySelector('.sh-send-err'), null, '不顯示錯誤列、不說「連不上」')
  assert.equal(document.querySelectorAll('.sh-msg').length, 2, '清單仍是第一頁的內容')
})
