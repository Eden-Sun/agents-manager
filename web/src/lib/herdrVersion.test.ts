import test from 'node:test'
import assert from 'node:assert/strict'
import { herdrVersionView } from './herdrVersion'
import { toHerdrVersion } from '../api/normalize'

const base = { server_version: '0.9.1', protocol: 22, protocol_supported: true, cli_version: '0.9.1', mismatch: false }

test('顯示 herdr 版本與 protocol', () => {
  const v = herdrVersionView(base)
  assert.equal(v.text, 'herdr 0.9.1 · protocol 22')
  assert.equal(v.level, 'ok')
})

test('CLI 與 server 不一致 → 警告並說明', () => {
  const v = herdrVersionView({ ...base, server_version: '0.8.2', protocol: 20, mismatch: true })
  assert.equal(v.level, 'warn')
  assert.ok(v.text.includes('0.8.2'))
  assert.ok(v.hint.includes('protocol_mismatch'))
})

test('讀不到 → 未知，不猜', () => {
  const v = herdrVersionView(toHerdrVersion(undefined))
  assert.equal(v.text, 'herdr 版本：未知')
  assert.equal(v.level, 'unknown')
  assert.ok(herdrVersionView(toHerdrVersion({ cli_version: '0.9.1' })).text.includes('server 版本未知'))
})

test('protocol 沒驗過 → 警告', () => {
  assert.equal(herdrVersionView({ ...base, protocol: 23, protocol_supported: false }).level, 'warn')
})
