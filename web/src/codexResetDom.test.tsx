import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from './testing/domHarness'
import { receiveCodexReset, useCodexReset } from './store/codexReset'
import { CodexResetHint } from './components/CodexResetHint'
before(setupDom)
afterEach(async () => { await unmountAll(); useCodexReset.setState({ info: null }); localStorage.clear() })
after(teardownDom)
function snapshot(id: string | null = 'reset:1', checked = new Date().toISOString()) {
  return { checked_at: checked, source: 'https://untrusted.example/', upcoming: false, last_reset_at: null,
    latest: null, notice: id ? { id, at: new Date().toISOString(), text: 'Global reset confirmed', url: 'javascript:alert(1)' } : null }
}
test('new announcement notifies once across state refresh, WS replay and browser reload', () => {
  const notices: string[] = []; const notify = (_kind: 'info', text: string) => notices.push(text)
  receiveCodexReset(snapshot(), notify)
  receiveCodexReset(snapshot(), notify)
  useCodexReset.setState({ info: null })
  receiveCodexReset(snapshot(), notify)
  assert.equal(notices.length, 1)
  assert.match(notices[0], /codex-reset.com/)
  receiveCodexReset(snapshot('reset:2'), notify)
  assert.equal(notices.length, 2)
  assert.equal(useCodexReset.getState().info?.source, 'https://codex-reset.com/')
})
test('baseline and expired announcements are silent; fresh announcement still arrives', () => {
  let count = 0; const notify = () => { count++ }
  receiveCodexReset(snapshot(null), notify)
  receiveCodexReset(snapshot('old', new Date(Date.now() - 80 * 60_000).toISOString()), notify)
  assert.equal(count, 0)
  const oldNotice = snapshot('old'); oldNotice.notice!.at = new Date(Date.now() - 49 * 3600_000).toISOString()
  receiveCodexReset(oldNotice, notify)
  assert.equal(count, 0)
  receiveCodexReset(snapshot('old'), notify)
  assert.equal(count, 1) // stale snapshot did not mark this announcement as delivered
  receiveCodexReset(snapshot('new'), notify)
  assert.equal(count, 2)
})
test('quota hint promises upcoming only for explicit signal and stops promising when stale', async () => {
  receiveCodexReset({ ...snapshot(null), probabilities: { rounded_24h: 99 } }, () => {})
  await mount(<CodexResetHint />)
  assert.match(document.querySelector('a')!.textContent!, /Codex 重置消息/)
  assert.doesNotMatch(document.querySelector('a')!.textContent!, /即將/)
  await act(async () => receiveCodexReset({ ...snapshot(null), upcoming: true }, () => {}))
  assert.match(document.querySelector('a')!.textContent!, /即將重置/)
  assert.match(document.querySelector('a')!.title, /尚未確認生效/)
  assert.equal(document.querySelector('a')!.href, 'https://codex-reset.com/')
  await act(async () => useCodexReset.setState({ info: { ...useCodexReset.getState().info!, checked_at: new Date(Date.now() - 80 * 60_000).toISOString() } }))
  assert.match(document.querySelector('a')!.textContent!, /資料過期/)
  assert.doesNotMatch(document.querySelector('a')!.textContent!, /即將/)
})
