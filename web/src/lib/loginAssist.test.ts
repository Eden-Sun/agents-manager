import test from 'node:test'
import assert from 'node:assert/strict'
import type { LoginStatus } from '../api'
import { canSubmitCode, loginHint, loginPhase, validLoginCode } from './loginAssist.ts'

const base: LoginStatus = {
  host: 'mac_builder',
  pane_id: 'w1:p2',
  identity: 'cc1',
  url: 'https://claude.com/oauth/authorize?a=b',
  awaiting_code: false,
  code_sent: false,
  failure: null,
  kind: 'claude',
  flow: 'code',
  device_code: null,
}

test('code 格式：跟 daemon 同一套（英數字與 _ . ~ # : / + = % -），空白與 shell 語法不收', () => {
  for (const ok of ['abc123', 'AbC-def_ghi.jkl~mno', 'code#state', ' a/b+c=d%20e:f ']) assert.equal(validLoginCode(ok), true, ok)
  for (const bad of ['', '   ', 'two words', 'a;b', '$(id)', '`x`', 'a|b', 'é', 'a'.repeat(1025)]) assert.equal(validLoginCode(bad), false, bad)
})

test('進度：沒網址→starting；有網址→url；畫面在等 code→waiting_code；送了→submitted；失敗→failed；pane 沒了→ended', () => {
  assert.equal(loginPhase({ ...base, url: null }, false), 'starting')
  assert.equal(loginPhase(base, false), 'url')
  assert.equal(loginPhase({ ...base, awaiting_code: true }, false), 'waiting_code')
  assert.equal(loginPhase({ ...base, code_sent: true }, false), 'submitted')
  assert.equal(loginPhase({ ...base, failure: 'Login failed: x' }, false), 'failed')
  assert.equal(loginPhase(base, true), 'ended')
  assert.equal(loginPhase(null, false), 'ended')
})

test('送出鈕只在畫面等 code、格式對、不在送出中才能按', () => {
  assert.equal(canSubmitCode('waiting_code', 'abc', false), true)
  assert.equal(canSubmitCode('waiting_code', 'abc', true), false)
  assert.equal(canSubmitCode('waiting_code', 'a b', false), false)
  assert.equal(canSubmitCode('waiting_code', '', false), false)
  for (const p of ['starting', 'url', 'submitted', 'failed', 'ended'] as const) assert.equal(canSubmitCode(p, 'abc', false), false, p)
})

test('提示文字：貼了不合格的字會講；每個階段都有一句', () => {
  assert.match(loginHint('waiting_code', 'a b'), /英數字/)
  assert.match(loginHint('waiting_code', ''), /貼在這裡/)
  for (const p of ['starting', 'url', 'submitted', 'failed', 'ended'] as const) assert.ok(loginHint(p, '').length > 0, p)
})

test('codex 裝置碼：網址與一次性碼都有才是 device（不貼回終端）；只有網址還是 starting；失敗照樣 failed', () => {
  const dev: LoginStatus = { ...base, url: 'https://auth.openai.com/codex/device', flow: 'device', device_code: 'AB7K-QX29', kind: 'codex' }
  assert.equal(loginPhase(dev, false), 'device')
  assert.equal(loginPhase({ ...dev, device_code: null }, false), 'starting')
  assert.equal(loginPhase({ ...dev, failure: 'Login failed: x' }, false), 'failed')
  assert.equal(canSubmitCode(loginPhase(dev, false), 'AB7K-QX29', false), false, '裝置碼流程沒有送 code 的輸入')
  assert.match(loginHint('device', ''), /不用貼回終端/)
})
