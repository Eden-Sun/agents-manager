import test, { beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import {
  clearRememberedTab,
  getRememberedTab,
  rememberTab,
  resolveInitialEnvTab,
  ENV_TABS,
  type EnvTabBadges,
} from './envTabs'

beforeEach(() => {
  clearRememberedTab()
})

test('七個分頁順序與名稱符合規格', () => {
  assert.deepEqual(
    ENV_TABS.map((t) => t.id),
    ['setup', 'hosts', 'updates', 'identities', 'judge', 'claude-api', 'openai-api', 'display'],
  )
})

test('第一次打開（無 remembered）：都沒有紅黃狀態字時停主機', () => {
  const badges: EnvTabBadges = {}
  assert.equal(resolveInitialEnvTab(badges, null), 'hosts')
})

test('第一次打開（無 remembered）：依導覽順序挑第一個有紅或黃狀態字的分頁', () => {
  const badges1: EnvTabBadges = {
    updates: { text: '2 可更新', tone: 'warn' },
  }
  assert.equal(resolveInitialEnvTab(badges1, null), 'updates')

  const badges2: EnvTabBadges = {
    hosts: { text: '1 未連線', tone: 'bad' },
    updates: { text: '2 可更新', tone: 'warn' },
  }
  assert.equal(resolveInitialEnvTab(badges2, null), 'hosts')

  const badges3: EnvTabBadges = {
    judge: { text: '需要注意', tone: 'warn' },
  }
  assert.equal(resolveInitialEnvTab(badges3, null), 'judge')
})

test('同一個 session 記住上次停的分頁：優先使用記住的分頁（不套用紅黃預設）', () => {
  const badges: EnvTabBadges = {
    hosts: { text: '1 未連線', tone: 'bad' },
  }

  rememberTab('claude-api')
  assert.equal(getRememberedTab(), 'claude-api')
  assert.equal(resolveInitialEnvTab(badges, getRememberedTab()), 'claude-api')

  rememberTab('display')
  assert.equal(getRememberedTab(), 'display')
  assert.equal(resolveInitialEnvTab(badges, getRememberedTab()), 'display')
})

test('sessionStorage 拋錯或不可用時有記憶體回退且不崩潰', () => {
  const originalSessionStorage = globalThis.sessionStorage
  Object.defineProperty(globalThis, 'sessionStorage', {
    value: {
      getItem() {
        throw new Error('SecurityError')
      },
      setItem() {
        throw new Error('SecurityError')
      },
      removeItem() {
        throw new Error('SecurityError')
      },
    },
    configurable: true,
  })

  try {
    clearRememberedTab()
    assert.equal(getRememberedTab(), null)
    rememberTab('updates')
    assert.equal(getRememberedTab(), 'updates')
  } finally {
    Object.defineProperty(globalThis, 'sessionStorage', {
      value: originalSessionStorage,
      configurable: true,
    })
  }
})
