import test from 'node:test'
import assert from 'node:assert/strict'
import { dispatchFrameForTest, useStore } from './store.ts'
import { reset } from './storeEnv.harness.ts'
import { resetAutoInstallNoticesForTest } from './integrationAutoInstall.ts'

const frame = (data: Record<string, unknown>) => dispatchFrameForTest({ type: 'herdr_integration_auto_install', data })

test('herdr_integration_auto_install：成功與失敗各跳自己的 toast，失敗帶原因；重播同一台同一家不再跳（#1871）', () => {
  reset()
  resetAutoInstallNoticesForTest()
  useStore.setState({ notices: [] })
  frame({ host: 'm4p', name: 'grok', ok: true, state: 'current', error: null })
  frame({ host: 'm4p', name: 'grok', ok: true, state: 'current', error: null })
  frame({ host: 'local', name: 'antigravity-cli', ok: false, state: null, error: 'permission denied' })
  const n = useStore.getState().notices
  assert.equal(n.length, 2, '重播不重複')
  assert.equal(n[0].kind, 'info')
  assert.equal(n[0].text, '已在 m4p 自動安裝 herdr grok integration')
  assert.equal(n[1].kind, 'error')
  assert.match(n[1].text, /本機 自動安裝 herdr antigravity-cli integration 失敗：permission denied.*不重試/)
  frame({ host: '', name: 'grok', ok: true })
  assert.equal(useStore.getState().notices.length, 2, '缺 host 的壞資料忽略')
})
