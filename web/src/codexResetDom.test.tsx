import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from './testing/domHarness'
import { receiveCodexReset, useCodexReset } from './store/codexReset'
import { CodexResetHint } from './components/CodexResetHint'
import { QuotaStrip } from './components/QuotaStrip'
import { resetStoreForTest, useStore } from './store/store'
before(setupDom)
afterEach(async () => { await unmountAll(); useCodexReset.setState({ info: null }); resetStoreForTest(); localStorage.clear() })
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
test('only a fresh official signal appears on the head; history and stale data stay in details', async () => {
  const now = Date.now()
  receiveCodexReset({ ...snapshot(null), probabilities: { rounded_24h: 99 } }, () => {})
  await mount(<><CodexResetHint now={now} /><CodexResetHint now={now} detail /></>)
  assert.equal(document.querySelector('.codex-reset-badge'), null)
  assert.match(document.querySelector('.codex-reset-detail')!.textContent!, /最近全域重置/)
  await act(async () => receiveCodexReset({ ...snapshot(null), upcoming: true }, () => {}))
  const badge = document.querySelector<HTMLAnchorElement>('.codex-reset-badge')!
  assert.match(badge.textContent!, /即將重置/)
  assert.match(badge.title, /尚未確認生效/)
  assert.equal(badge.href, 'https://codex-reset.com/')
  await act(async () => useCodexReset.setState({ info: { ...useCodexReset.getState().info!, checked_at: new Date(now - 80 * 60_000).toISOString() } }))
  assert.equal(document.querySelector('.codex-reset-badge'), null)
  assert.match(document.querySelector('.codex-reset-detail')!.textContent!, /資料過期/)
  assert.doesNotMatch(document.querySelector('.codex-reset-detail')!.textContent!, /即將/)
})
test('each Codex cell owns its badge and detail; source clicks do not open or disable quota cells', async () => {
  resetStoreForTest()
  const win = { used_pct: 20, low: false, critical: false, resets_at: new Date(Date.now() + 3600_000).toISOString(), observed_at: null }
  const quota = Object.fromEntries(['claude', 'codex', 'codex:work'].map((key) => [key, { five_hour: win, seven_day: win, fable: null, updated_at: new Date().toISOString(), stale: false, host: 'local' }]))
  useStore.setState({ quota, identities: [{ name: 'work', kind: 'codex', env: { CODEX_HOME: '/tmp/test-work' }, args: [], host: null }] } as never)
  receiveCodexReset({ ...snapshot(null), upcoming: true }, () => {})
  await mount(<QuotaStrip focusKind="codex" />)
  assert.equal(document.querySelectorAll('.quota-hp.codex .quota-head .codex-reset-badge').length, 2)
  assert.equal(document.querySelector('.quota-hp.claude .codex-reset-badge'), null)
  assert.equal(document.querySelector('.quota-open > .codex-reset-badge'), null)
  const badge = document.querySelector<HTMLAnchorElement>('.codex-reset-badge')!
  badge.addEventListener('click', (e) => e.preventDefault())
  await act(async () => badge.click())
  assert.equal(document.querySelector('.quota-pop'), null)
  await act(async () => document.querySelector<HTMLButtonElement>('.quota-hp.codex .quota-bars-open')!.click())
  const details = document.querySelectorAll<HTMLAnchorElement>('.quota-pop .codex-reset-detail')
  assert.equal(details.length, 2)
  for (const link of details) {
    assert.ok(link.closest('.quota-pop-row')?.querySelector('.quota-kind.codex'))
    assert.match(link.textContent!, /codex-reset.com/)
    link.addEventListener('click', (e) => e.preventDefault())
    await act(async () => link.click())
    assert.equal(link.closest('.quota-pop-row')!.classList.contains('off'), false)
  }
  await act(async () => useCodexReset.setState({ info: { ...useCodexReset.getState().info!, upcoming: false } }))
  assert.equal(document.querySelectorAll('.codex-reset-badge').length, 0)
  assert.equal(document.querySelectorAll('.codex-reset-detail').length, 2)
  await act(async () => {
    useCodexReset.setState({ info: { ...useCodexReset.getState().info!, upcoming: true } })
    useStore.setState({ quota: { claude: quota.claude }, identities: [] } as never)
  })
  assert.equal(document.querySelector('.quota-hp.codex'), null)
  assert.equal(document.querySelector('.codex-reset-badge'), null)
  assert.equal(document.querySelector('.codex-reset-detail'), null)
})
