import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll, until, settle, act } from '../testing/domHarness'
import { MessageAttachments } from './Attachments'
import { useStore } from '../store/store'

const originalFetch = globalThis.fetch

before(setupDom)
afterEach(async () => {
  await unmountAll()
  globalThis.fetch = originalFetch
})
after(teardownDom)

test('斷線時抓失敗：連回來自動重抓，縮圖換成圖', async () => {
  const att = { id: 'att-retry-1', name: 'a.png', mime: 'image/png', size: 4, path: '/x/a.png' }
  let shouldSucceed = false
  let callCount = 0

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    if (String(input).endsWith(`/api/attachments/${att.id}`)) {
      callCount += 1
      if (!shouldSucceed) {
        return new Response('', { status: 502 })
      }
      return new Response(new Blob([new Uint8Array([1])], { type: 'image/png' }), { status: 200 })
    }
    return new Response('{}', { status: 200 })
  }) as unknown as typeof fetch

  await act(async () => {
    useStore.setState({ socket: 'closed' })
  })
  await mount(<MessageAttachments items={[att]} />)

  await until(
    () => {
      const el = document.querySelector<HTMLElement>('.msg-attachment.failed')
      return Boolean(el && el.textContent?.includes('無法載入'))
    },
    '等待無法載入出現',
  )

  shouldSucceed = true
  await act(async () => {
    useStore.setState({ socket: 'open' })
  })

  await until(
    () => {
      const img = document.querySelector('.msg-attachment img')
      const failed = document.querySelector('.msg-attachment.failed')
      return Boolean(img && !failed)
    },
    '等待縮圖重抓成功並換成圖片',
  )
  assert.ok(callCount >= 2, '至少呼叫兩次')
})

test('已清除（404）不重試', async () => {
  const att = { id: 'att-gone-2', name: 'gone.png', mime: 'image/png', size: 4, path: '/x/gone.png' }
  let callCount = 0

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    if (String(input).endsWith(`/api/attachments/${att.id}`)) {
      callCount += 1
      return new Response(JSON.stringify({ error: 'not_found' }), { status: 404 })
    }
    return new Response('{}', { status: 200 })
  }) as unknown as typeof fetch

  await act(async () => {
    useStore.setState({ socket: 'closed' })
  })
  await mount(<MessageAttachments items={[att]} />)

  await until(
    () => {
      const el = document.querySelector<HTMLElement>('.msg-attachment.failed')
      return Boolean(el && el.textContent?.includes('已清除'))
    },
    '等待已清除出現',
  )

  const n = callCount
  await act(async () => {
    useStore.setState({ socket: 'open' })
  })
  await settle(100)

  assert.equal(callCount, n, '404 不會因為 socket 連上而重試')
})

test('連線中一直失敗不會無限重試', async () => {
  const att = { id: 'att-fail-3', name: 'fail.png', mime: 'image/png', size: 4, path: '/x/fail.png' }
  let callCount = 0

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    if (String(input).endsWith(`/api/attachments/${att.id}`)) {
      callCount += 1
      return new Response('', { status: 502 })
    }
    return new Response('{}', { status: 200 })
  }) as unknown as typeof fetch

  await act(async () => {
    useStore.setState({ socket: 'open' })
  })
  await mount(<MessageAttachments items={[att]} />)

  await until(
    () => {
      const el = document.querySelector<HTMLElement>('.msg-attachment.failed')
      return Boolean(el && el.textContent?.includes('無法載入'))
    },
    '等待無法載入出現',
  )

  await settle(300)
  assert.ok(callCount <= 2, `失敗次數不得形成無限重試：目前為 ${callCount}`)
})
