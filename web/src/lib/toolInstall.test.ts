import test from 'node:test'
import assert from 'node:assert/strict'
import { baselineWarnWhy, toolInstallInfo } from './toolInstall'

test('Darwin 只列 macOS 指令；Linux 只列 Linux 指令', () => {
  assert.deepEqual(toolInstallInfo('gh', 'Darwin')?.steps, [{ label: 'macOS', command: 'brew install gh' }])
  assert.deepEqual(toolInstallInfo('gh', 'Linux')?.steps, [{ label: 'Linux', command: 'sudo apt install gh' }])
})

test('OS 不明時兩套都列，連結照給', () => {
  const v = toolInstallInfo('bun', null)
  assert.equal(v?.steps.length, 2)
  assert.equal(v?.link, 'https://bun.sh')
})

test('rtk 沒有經過核對的安裝來源：不給指令、不給連結', () => {
  const v = toolInstallInfo('rtk', 'Linux')
  assert.deepEqual(v?.steps, [])
  assert.equal(v?.link, null)
})

test('清單外的工具回 null', () => {
  assert.equal(toolInstallInfo('nope', 'Linux'), null)
})

test('warn 項有說明；沒列到的 id 回 null', () => {
  assert.match(baselineWarnWhy('codex.config.toml') ?? '', /內建預設/)
  assert.match(baselineWarnWhy('codex.hooks.json') ?? '', /hook/)
  assert.match(baselineWarnWhy('codex.config.toml:features.hooks') ?? '', /features.hooks/)
  assert.match(baselineWarnWhy('grok.config.toml') ?? '', /Grok/)
  assert.match(baselineWarnWhy('herdr.config.toml') ?? '', /herdr/)
  assert.equal(baselineWarnWhy('claude.hooks'), null)
})
