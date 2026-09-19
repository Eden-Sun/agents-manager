import test from 'node:test'
import assert from 'node:assert/strict'
import { buildRoute, HOME, parseRoute, screenKey, type Route } from './routes.ts'

/** build 一趟再 parse 回來要拿到同一個 Route。 */
const roundTrip = (r: Route) => parseRoute(buildRoute(r))

test('parse/build: 每個畫面都對稱', () => {
  const routes: Route[] = [
    HOME,
    { kind: 'bot', botId: 'b1', tab: 'chat', settings: false },
    { kind: 'bot', botId: 'b1', tab: 'terminal', settings: false },
    { kind: 'bot', botId: 'b1', tab: 'preview', settings: false },
    { kind: 'bot', botId: 'b1', tab: 'chat', settings: true },
    { kind: 'project', projectId: 'p1' },
    { kind: 'shell', host: 'local', paneId: 'pane-9' },
  ]
  for (const r of routes) assert.deepEqual(roundTrip(r), r, buildRoute(r))
})

test('build: goal 表格裡的路徑就是這些字串', () => {
  assert.equal(buildRoute({ kind: 'bot', botId: 'b1', tab: 'chat', settings: false }), '/bots/b1')
  assert.equal(buildRoute({ kind: 'bot', botId: 'b1', tab: 'terminal', settings: false }), '/bots/b1/terminal')
  assert.equal(buildRoute({ kind: 'bot', botId: 'b1', tab: 'preview', settings: false }), '/bots/b1/preview')
  assert.equal(buildRoute({ kind: 'bot', botId: 'b1', tab: 'chat', settings: true }), '/bots/b1/settings')
  assert.equal(buildRoute({ kind: 'project', projectId: 'p1' }), '/projects/p1')
  assert.equal(buildRoute({ kind: 'shell', host: 'mini', paneId: 'w1:t2:p3' }), '/hosts/mini/shells/w1%3At2%3Ap3')
})

test('parse: 設定路徑一定停在對話分頁（兩者不會同時成立）', () => {
  assert.deepEqual(parseRoute('/bots/b1/settings'), { kind: 'bot', botId: 'b1', tab: 'chat', settings: true })
})

test('parse: 認不得的路徑一律回首頁', () => {
  const bad = [
    '/nope',
    '/bots',
    '/bots/b1/nope',
    '/bots/b1/terminal/extra',
    '/projects',
    '/projects/p1/extra',
    '/hosts/local/shells',
    '/hosts/local/panes/p1',
    '/hosts/local/shells/p1/extra',
  ]
  for (const p of bad) assert.deepEqual(parseRoute(p), HOME, p)
})

test('parse: 前後多餘的斜線與尾斜線不算另一個畫面', () => {
  assert.deepEqual(parseRoute('/bots/b1/'), { kind: 'bot', botId: 'b1', tab: 'chat', settings: false })
  assert.deepEqual(parseRoute('//bots//b1//'), { kind: 'bot', botId: 'b1', tab: 'chat', settings: false })
  assert.deepEqual(parseRoute(''), HOME)
})

test('parse: id 逸出過就要解回來，解不開的照原樣（不丟例外）', () => {
  assert.deepEqual(parseRoute('/hosts/mini/shells/w1%3At2'), { kind: 'shell', host: 'mini', paneId: 'w1:t2' })
  assert.deepEqual(parseRoute('/bots/b%201'), { kind: 'bot', botId: 'b 1', tab: 'chat', settings: false })
  assert.deepEqual(parseRoute('/bots/b%ZZ'), { kind: 'bot', botId: 'b%ZZ', tab: 'chat', settings: false })
})

test('screenKey: 對話↔終端是同一格，設定另開一格', () => {
  const chat: Route = { kind: 'bot', botId: 'b1', tab: 'chat', settings: false }
  const term: Route = { kind: 'bot', botId: 'b1', tab: 'terminal', settings: false }
  const settings: Route = { kind: 'bot', botId: 'b1', tab: 'chat', settings: true }
  assert.equal(screenKey(chat), screenKey(term))
  assert.notEqual(screenKey(chat), screenKey(settings))
  assert.notEqual(screenKey(chat), screenKey({ kind: 'bot', botId: 'b2', tab: 'chat', settings: false }))
  assert.notEqual(screenKey({ kind: 'project', projectId: 'x' }), screenKey({ kind: 'bot', botId: 'x', tab: 'chat', settings: false }))
})
