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
  assert.equal(isMobilePresence({ matchMedia: (q) => ({ matches: q.includes('coarse') || q.includes('max-width: 768px') }) }), true)
  assert.equal(isMobilePresence({ matchMedia: () => ({ matches: false }) }), false)
  assert.equal(isMobilePresence({} as unknown as Window), false)
})

test('installPresenceReporter：回報帶 mobile、visibilitychange 與 pagehide 觸發回報', async () => {
  const origRequest = rawTransport.request
  const origMatchMedia = window.matchMedia
  const reports: Array<Record<string, unknown>> = []

  rawTransport.request = (async (_method: string, path: string, body?: unknown) => {
    if (path === '/push/presence' && body && typeof body === 'object') {
      reports.push(body as Record<string, unknown>)
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
  const stop = installPresenceReporter(() => currentBot)

  assert.equal(reports.length, 1)
  assert.equal(reports[0].bot_id, 'bot-1')
  assert.equal(reports[0].mobile, true)
  assert.equal(typeof reports[0].client_id, 'string')

  currentBot = 'bot-2'
  document.dispatchEvent(new Event('visibilitychange'))
  assert.equal(reports.length, 2)
  assert.equal(reports[1].bot_id, 'bot-2')
  assert.equal(reports[1].mobile, true)

  window.dispatchEvent(new Event('pagehide'))
  assert.equal(reports.length, 3)
  assert.equal(reports[2].visible, false)
  assert.equal(reports[2].mobile, true)

  stop()

  document.dispatchEvent(new Event('visibilitychange'))
  window.dispatchEvent(new Event('pagehide'))
  assert.equal(reports.length, 3)

  rawTransport.request = origRequest
  window.matchMedia = origMatchMedia
})
