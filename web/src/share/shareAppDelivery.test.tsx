/**
 * #922：分享頁要看 `POST /messages` 回的 `delivery`／`message_id`——
 * `failed` 時不能把字清掉又卡在「思考中」；回覆的判斷用 `message_id` 當錨點、不比手機時鐘；
 * 最後還有 90 秒的出口，SSE 與輪詢都漏掉時送出鈕不會永遠灰著。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { installManualTimers } from '../testing/manualTimers'
import { ShareApp } from './ShareApp'
import type { ShareClient, ShareEvents, ShareSendResult } from './shareApi'
import type { ShareMessage, ShareStatus } from './shareModel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const box = () => document.querySelector('textarea') as HTMLTextAreaElement
const sendBtn = () => document.querySelector('.sh-send') as HTMLButtonElement
const thinking = () => document.querySelector('.sh-thinking')
const err = () => document.querySelector('.sh-send-err')?.textContent ?? ''

function client(opts: {
  send: () => ShareSendResult | void
  page: () => { status: ShareStatus; messages: ShareMessage[] }
  down?: boolean
}): ShareClient {
  return {
    async messages() {
      return { bot_name: 'b', has_more: false, ...opts.page() }
    },
    async send() {
      return opts.send()
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
    subscribe(ev: ShareEvents) {
      // 推播起不來：只能靠輪詢（`onDown`）；沒設就是連上了但什麼事件都不會來。
      if (opts.down) ev.onDown()
      else ev.onUp?.()
      return () => {}
    },
  }
}

const msg = (id: string, role: 'user' | 'assistant', created_at: string): ShareMessage => ({ id, role, text: id, created_at, attachments: [] })

test('delivery=failed：思考中不出現、送出鈕沒灰、字還在、有錯誤提示', async () => {
  await mount(
    <ShareApp
      client={client({
        send: () => ({ delivery: 'failed', messageId: 'u1' }),
        page: () => ({ status: 'idle', messages: [msg('u1', 'user', new Date().toISOString())] }),
      })}
    />,
  )
  await settle(20)
  await typeInto(box(), '這一則 bot 沒收到')
  await click(sendBtn())
  await settle(50)
  assert.equal(thinking(), null, 'failed 不能停在思考中')
  assert.equal(box().value, '這一則 bot 沒收到', '字不能被清掉')
  assert.match(err(), /bot 沒收到/)
  assert.equal(sendBtn().disabled, false, '送出鈕要能再按')
})

test('delivery=unknown：保留思考中、字已送出，但提示還不確定', async () => {
  await mount(
    <ShareApp
      client={client({
        send: () => ({ delivery: 'unknown', messageId: 'u1' }),
        page: () => ({ status: 'idle', messages: [msg('u1', 'user', new Date().toISOString())] }),
      })}
    />,
  )
  await settle(20)
  await typeInto(box(), '不確定有沒有收到')
  await click(sendBtn())
  await settle(50)
  assert.ok(thinking(), 'unknown 先當作還在處理')
  assert.equal(box().value, '')
  assert.match(err(), /不確定/)
})

test('用 message_id 當錨點：回覆的 created_at 比手機時鐘早 10 分鐘，輪詢看到仍清掉思考中', { timeout: 20_000 }, async () => {
  const early = new Date(Date.now() - 10 * 60_000).toISOString()
  let replied = false
  await mount(
    <ShareApp
      client={client({
        send: () => ({ delivery: 'sent', messageId: 'u1' }),
        page: () => ({
          status: 'idle',
          messages: replied ? [msg('u1', 'user', early), msg('a1', 'assistant', early)] : [msg('u1', 'user', early)],
        }),
        down: true,
      })}
    />,
  )
  await settle(20)
  await typeInto(box(), '手機時鐘比 daemon 快')
  await click(sendBtn())
  await settle(50)
  assert.ok(thinking(), '回覆還沒來')
  replied = true
  await settle(4400) // 輪詢（POLL_MS 4000）抓到 u1 之後的 assistant
  assert.equal(thinking(), null, 'u1 之後有 assistant 就清掉，不比 created_at')
})

test('90 秒上限：送出成功但永遠沒有 status／回覆，送出鈕恢復並提示', async () => {
  await mount(
    <ShareApp
      client={client({
        send: () => ({ delivery: 'sent', messageId: 'u1' }),
        page: () => ({ status: 'idle', messages: [msg('u1', 'user', new Date().toISOString())] }),
      })}
    />,
  )
  await settle(20)
  await typeInto(box(), '永遠沒有回覆')
  const timers = installManualTimers()
  try {
    await click(sendBtn())
    await act(async () => {})
    assert.ok(thinking(), '送出後在等')
    await act(async () => timers.clock.advance(89_000))
    assert.ok(thinking(), '89 秒還在等')
    await act(async () => timers.clock.advance(2_000))
    assert.equal(thinking(), null, '超過 90 秒放開')
    assert.match(err(), /沒等到回應/)
  } finally {
    timers.restore()
  }
  await typeInto(box(), '再送一次')
  assert.equal(sendBtn().disabled, false, '送出鈕恢復')
})
