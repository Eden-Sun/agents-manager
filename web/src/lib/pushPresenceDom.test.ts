import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { setupDom, teardownDom } from '../testing/domHarness'
import { installPresenceReporter, isMobilePresence } from './pushNotify'
import { rawTransport } from '../api'

before(async () => {
  await setupDom()
})
after(async () => {
  await teardownDom()
})

test('isMobilePresence：窄螢幕或粗指標均視為手機，桌機與缺 matchMedia 視為否', () => {
  assert.equal(isMobilePresence({ matchMedia: (q) => ({ matches: q.includes('max-width: 640px') }) }), true)
  assert.equal(isMobilePresence({ matchMedia: (q) => ({ matches: q.includes('coarse') || q.includes('max-width: 1024px') }) }), true)
  assert.equal(isMobilePresence({ matchMedia: () => ({ matches: false }) }), false)
  assert.equal(isMobilePresence({} as unknown as Window), false)
})

test('installPresenceReporter：回報帶 mobile、pageshow/focus/visibilitychange/pagehide 觸發回報與失敗短重試', async () => {
  const origRequest = rawTransport.request
  const origMatchMedia = window.matchMedia
  const reports: Array<Record<string, unknown>> = []
  let failOnce = false

  rawTransport.request = (async (_method: string, path: string, body?: unknown) => {
    if (path === '/push/presence' && body && typeof body === 'object') {
      reports.push(body as Record<string, unknown>)
      if (failOnce) {
        failOnce = false
        throw new Error('network waking up')
      }
    }
    return { ok: true }
  }) as typeof rawTransport.request

  window.matchMedia = ((q: string) => ({
    matches: q.includes('max-width: 640px'),
    media: q,
    addEventListener() {},
    removeEventListener() {},
  })) as unknown as typeof window.matchMedia

  let currentBot: string | null = 'bot-1'
  const stop = installPresenceReporter(() => currentBot, 20)

  assert.equal(reports.length, 1)
  assert.equal(reports[0].bot_id, 'bot-1')
  assert.equal(reports[0].mobile, true)
  assert.equal(typeof reports[0].client_id, 'string')

  currentBot = 'bot-2'
  document.dispatchEvent(new Event('visibilitychange'))
  assert.equal(reports.length, 2)
  assert.equal(reports[1].bot_id, 'bot-2')
  assert.equal(reports[1].mobile, true)

  window.dispatchEvent(new Event('pageshow'))
  window.dispatchEvent(new Event('focus'))
  assert.equal(reports.length, 4)
  assert.equal(reports[2].visible, true)
  assert.equal(reports[3].visible, true)

  failOnce = true
  window.dispatchEvent(new Event('focus'))
  assert.equal(reports.length, 5)
  await new Promise((r) => setTimeout(r, 60))
  assert.equal(reports.length, 6, '前景回報失敗應在短退避後重試')

  window.dispatchEvent(new Event('pagehide'))
  assert.equal(reports.length, 7)
  assert.equal(reports[6].visible, false)
  assert.equal(reports[6].mobile, true)

  stop()

  document.dispatchEvent(new Event('visibilitychange'))
  window.dispatchEvent(new Event('pageshow'))
  window.dispatchEvent(new Event('focus'))
  window.dispatchEvent(new Event('pagehide'))
  assert.equal(reports.length, 7)

  rawTransport.request = origRequest
  window.matchMedia = origMatchMedia
})
