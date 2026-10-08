/**
 * #921：分享頁送出的 `client_request_id` 要活過「回應遺失」——網路斷在 daemon 收下之後，使用者照提示再按一次，
 * 兩次 POST 帶同一個鍵，daemon 才認得那是重送而不是第二則。daemon 明確回了（ShareHttpError）或改了字就是新的鍵。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import type { ShareClient, ShareSendResult } from './shareApi'
import { ShareHttpError, type ShareMessage } from './shareModel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

interface Sent {
  text: string
  crid: string
  attachments: string[]
}

/** `outcomes[i]` 是第 i 次 `send` 的結果：Error 就 reject，其餘 resolve。 */
function fakeClient(outcomes: Array<Error | ShareSendResult | void>): { client: ShareClient; sent: Sent[] } {
  const sent: Sent[] = []
  // 每次成功的送出，bot 立刻回覆：`load()` 之後「思考中」就清掉，下一次才送得出去。
  const log: ShareMessage[] = []
  const client: ShareClient = {
    async messages() {
      return { bot_name: 'b', status: 'idle', messages: [...log], has_more: false }
    },
    async send(text, crid, attachments) {
      sent.push({ text, crid, attachments })
      const out = outcomes[sent.length - 1]
      if (out instanceof Error) throw out
      const id = out?.messageId ?? `u${sent.length}`
      const at = new Date().toISOString()
      log.push({ id, role: 'user', text, created_at: at, attachments: [] }, { id: `${id}-a`, role: 'assistant', text: '好', created_at: at, attachments: [] })
      return out
    },
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
  return { client, sent }
}

const box = () => document.querySelector('textarea') as HTMLTextAreaElement
const sendBtn = () => document.querySelector('.sh-send') as HTMLButtonElement
const err = () => document.querySelector('.sh-send-err')?.textContent ?? ''

test('回應遺失（fetch 丟 TypeError）後照提示再按一次：兩次送出同一個 client_request_id，成功後輸入框清空', async () => {
  const { client, sent } = fakeClient([new TypeError('Failed to fetch'), { delivery: 'sent', messageId: 'u1' }])
  await mount(<ShareApp client={client} />)
  await settle(20)
  await typeInto(box(), '重送測試：網路斷在 daemon 收下之後')
  await click(sendBtn())
  await settle(20)
  assert.match(err(), /送出失敗/)
  assert.equal(box().value, '重送測試：網路斷在 daemon 收下之後', '字還在')
  await click(sendBtn())
  await settle(20)
  assert.equal(sent.length, 2)
  assert.equal(sent[0].crid, sent[1].crid, '重送沿用同一個鍵')
  assert.match(sent[0].crid, /^share-[A-Za-z0-9\-_.:]{1,57}$/, '字元集與長度符合 daemon 的限制')
  assert.equal(box().value, '')
})

test('daemon 明確回了錯誤（409）：作廢那個鍵，再按一次是新的 client_request_id', async () => {
  const { client, sent } = fakeClient([new ShareHttpError(409), { delivery: 'sent', messageId: 'u2' }])
  await mount(<ShareApp client={client} />)
  await settle(20)
  await typeInto(box(), '重送測試：被 409 擋下')
  await click(sendBtn())
  await settle(20)
  assert.match(err(), /還沒回完/)
  await click(sendBtn())
  await settle(20)
  assert.equal(sent.length, 2)
  assert.notEqual(sent[0].crid, sent[1].crid, 'daemon 明確回了，下一次是新的動作')
})

test('改了字再送：新的 client_request_id；成功後同一句再送一次也是新的', async () => {
  const { client, sent } = fakeClient([new TypeError('Failed to fetch'), { delivery: 'sent', messageId: 'u3' }, { delivery: 'sent', messageId: 'u4' }])
  await mount(<ShareApp client={client} />)
  await settle(20)
  await typeInto(box(), '重送測試：第一句')
  await click(sendBtn())
  await settle(20)
  await typeInto(box(), '重送測試：換成第二句')
  await click(sendBtn())
  await settle(20)
  assert.equal(sent.length, 2)
  assert.notEqual(sent[0].crid, sent[1].crid, '字不一樣就是另一則')
  // 第二句成功之後，使用者又打一模一樣的話：是新的一則，不能被當成重送。
  await typeInto(box(), '重送測試：換成第二句')
  await click(sendBtn())
  await settle(20)
  assert.equal(sent.length, 3)
  assert.notEqual(sent[1].crid, sent[2].crid, '成功就作廢，下一次同一句是新的動作')
})
