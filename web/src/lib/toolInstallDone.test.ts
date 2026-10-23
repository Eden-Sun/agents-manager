import test from 'node:test'
import assert from 'node:assert/strict'
import { toToolInstallDone, toolInstallDoneText } from './toolInstallDone'

const ev = (o: Record<string, unknown>) => toToolInstallDone({ install_id: 'attempt-1', outcome: 'failed', host: 'local', kind: 'claude', installed: false, pane_id: 'w9:s1', pane_kept: true, timed_out: false, ...o })!

test('不認得的 kind／壞形狀不處理', () => {
  assert.equal(toToolInstallDone(null), null)
  assert.equal(toToolInstallDone({ kind: 'nope' }), null)
})

test('裝成功：info，不開 pane', () => {
  const r = toolInstallDoneText(ev({ installed: true, pane_kept: false }))
  assert.equal(r.level, 'info')
  assert.equal(r.openPane, false)
})

test('沒裝成且 pane 還在：error 並開 pane；逾時要講清楚是逾時', () => {
  const a = toolInstallDoneText(ev({}))
  assert.equal(a.level, 'error')
  assert.equal(a.openPane, true)
  assert.match(a.text, /錯誤輸出在裡面/)
  const t = toolInstallDoneText(ev({ timed_out: true }))
  assert.match(t.text, /15 分鐘.*逾時/)
  assert.equal(t.openPane, true)
})

test('沒裝成而且 pane 被使用者關了：不開 pane，叫他再按安裝', () => {
  const r = toolInstallDoneText(ev({ pane_kept: false }))
  assert.equal(r.openPane, false)
  assert.match(r.text, /再按一次「安裝」/)
})


test('逾時優先於 installed，換代結果不開 pane', () => {
  const timeout = toolInstallDoneText(ev({ installed: true, timed_out: true }))
  assert.equal(timeout.level, 'error')
  assert.match(timeout.text, /逾時/)
  const stale = toolInstallDoneText(ev({ outcome: 'superseded', installed: true }))
  assert.equal(stale.openPane, false)
  assert.match(stale.text, /失聯/)
})
