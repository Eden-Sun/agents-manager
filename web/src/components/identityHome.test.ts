import test from 'node:test'
import assert from 'node:assert/strict'
import type { BotKind } from '../api/types'
import { identityHomeDir, normalizeHomeDir, sharedHomeWith, suggestedIdentityEnvText, validIdentityName } from './identityEnv'

test('建議的目錄：每個 CLI 用自己的目錄變數，名稱接在後面；沒有目錄變數的 kind 不給建議', () => {
  assert.equal(suggestedIdentityEnvText('claude', 'cc1'), 'CLAUDE_CONFIG_DIR=$HOME/.claude-cc1')
  assert.equal(suggestedIdentityEnvText('codex', 'work'), 'CODEX_HOME=$HOME/.codex-work')
  assert.equal(suggestedIdentityEnvText('grok', 'g2'), 'GROK_HOME=$HOME/.grok-g2')
  assert.equal(suggestedIdentityEnvText('agy', 'a1'), 'HOME=$HOME/.agy-a1')
  assert.equal(suggestedIdentityEnvText('claude-api', 'x'), '')
})

test('名稱規則同 daemon：小寫英文字母開頭、最多 32 字', () => {
  assert.ok(validIdentityName('cc1'))
  assert.ok(validIdentityName('work-2_x'))
  assert.ok(!validIdentityName('CC1'))
  assert.ok(!validIdentityName('1cc'))
  assert.ok(!validIdentityName(''))
  assert.ok(!validIdentityName('a'.repeat(33)))
})

test('目錄比對：~、$HOME、${HOME} 視為同一個家目錄，尾端 / 不影響', () => {
  assert.equal(normalizeHomeDir('~/.claude-cc1/'), '$HOME/.claude-cc1')
  assert.equal(normalizeHomeDir('${HOME}/.claude-cc1'), '$HOME/.claude-cc1')
  assert.equal(normalizeHomeDir('$HOME/.claude-cc1'), '$HOME/.claude-cc1')
  assert.equal(normalizeHomeDir('$HOME/.claude-cc10'), '$HOME/.claude-cc10', '只比完整段，不做前綴比對')
})

test('身份目錄：取出目錄變數的值；沒有或空白就是 null', () => {
  assert.equal(identityHomeDir('claude', { CLAUDE_CONFIG_DIR: ' $HOME/.claude-cc1 ' }), '$HOME/.claude-cc1')
  assert.equal(identityHomeDir('claude', { CODEX_HOME: '$HOME/.codex' }), null)
  assert.equal(identityHomeDir('claude', { CLAUDE_CONFIG_DIR: '   ' }), null)
})

test('與既有身份共用目錄：只看同 kind、同一台主機；指到同一目錄就列出名稱', () => {
  const existing: { name: string; kind: BotKind; host: string | null; env: Record<string, string> }[] = [
    { name: 'cc1', kind: 'claude', host: null, env: { CLAUDE_CONFIG_DIR: '~/.claude-cc1' } },
    { name: 'cc2', kind: 'claude', host: 'm4p', env: { CLAUDE_CONFIG_DIR: '$HOME/.claude-cc2' } },
    { name: 'cx1', kind: 'codex', host: null, env: { CODEX_HOME: '$HOME/.claude-cc3' } },
  ]
  const r = sharedHomeWith('claude', { CLAUDE_CONFIG_DIR: '$HOME/.claude-cc1/' }, 'local', existing)
  assert.deepEqual(r, { names: ['cc1'], defaultAccount: false })
  // cc2 在 m4p，不是本機的：同一路徑在別台機器是另一份登入。
  assert.deepEqual(sharedHomeWith('claude', { CLAUDE_CONFIG_DIR: '$HOME/.claude-cc2' }, 'local', existing).names, [])
  assert.deepEqual(sharedHomeWith('claude', { CLAUDE_CONFIG_DIR: '$HOME/.claude-cc2' }, 'm4p', existing).names, ['cc2'])
  // 不同 kind 的同名路徑不算。
  assert.deepEqual(sharedHomeWith('claude', { CLAUDE_CONFIG_DIR: '$HOME/.claude-cc3' }, 'local', existing).names, [])
})

test('與預設帳號共用：指到 ~/.claude（claude）就標 defaultAccount', () => {
  assert.deepEqual(sharedHomeWith('claude', { CLAUDE_CONFIG_DIR: '~/.claude' }, 'local', []), { names: [], defaultAccount: true })
  assert.deepEqual(sharedHomeWith('codex', { CODEX_HOME: '$HOME/.codex/' }, 'local', []), { names: [], defaultAccount: true })
  assert.deepEqual(sharedHomeWith('claude', { CLAUDE_CONFIG_DIR: '$HOME/.claude-cc1' }, 'local', []), { names: [], defaultAccount: false })
})
