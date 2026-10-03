/**
 * 分享頁（SPEC「分享 bot」）：對話、送出後「思考中」→ 回覆、上傳附件跟著訊息送、bot 給的檔案清單、連結失效畫面；
 * 頁面上沒有任何通往主 UI 的連結。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import { mockShareClient } from './shareMock'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const TOKEN = 'demo_share_token_0123456789'

test('載入對話與檔案；沒有通往主 UI 的連結', async () => {
  await mount(<ShareApp client={mockShareClient(TOKEN)} />)
  await settle(300)
  assert.equal(document.querySelector('.sh-title h1')!.textContent, 'support-bot')
  assert.equal(document.querySelectorAll('.sh-msg').length, 3)
  assert.deepEqual([...document.querySelectorAll('.sh-file-name')].map((x) => x.textContent), ['config.toml', '安裝步驟.md'])
  for (const a of document.querySelectorAll('a')) {
    const href = a.getAttribute('href') ?? ''
    assert.ok(href.startsWith('blob:') || href.startsWith('/s/') || /^https?:\/\//.test(href), `可疑連結 ${href}`)
    assert.ok(!/\/api\/(state|bots|projects)|:7788/.test(href), `不能連到主 UI：${href}`)
  }
})

test('送出：輸入框清空、出現思考中，回覆到了思考中消失', async () => {
  await mount(<ShareApp client={mockShareClient(TOKEN)} />)
  await settle(300)
  const ta = document.querySelector('textarea')!
  await act(async () => {
    const set = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(ta), 'value')!.set!
    set.call(ta, '請問退款怎麼申請？')
    ta.dispatchEvent(new Event('input', { bubbles: true }))
  })
  await click(document.querySelector('.sh-send')!)
  await settle(500)
  assert.equal(ta.value, '')
  assert.ok(document.querySelector('.sh-thinking'), '送出後要看得到思考中')
  assert.match([...document.querySelectorAll('.sh-msg.user')].at(-1)!.textContent!, /退款/)
  await settle(2600)
  assert.equal(document.querySelector('.sh-thinking'), null)
  assert.match([...document.querySelectorAll('.sh-msg.assistant')].at(-1)!.textContent!, /mock 的回覆/)
})

test('連結失效（404）：只有失效說明，沒有輸入框', async () => {
  await mount(<ShareApp client={mockShareClient('expired_0123456789abcdef')} />)
  await settle(300)
  assert.match(document.body.textContent!, /這個分享連結已失效/)
  assert.equal(document.querySelector('textarea'), null)
})
