import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, typeInto, unmountAll, until } from '../testing/domHarness'
import { AddIdentityDialog } from './AddIdentityDialog'
import type { AddIdentityDialogProps } from './AddIdentityDialog'
import type { Identity, NewIdentityInput } from '../api/types'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const LOCAL = [{ name: 'local', label: '本機', connected: true }]
const EXISTING: Identity[] = [{ name: 'cc1', kind: 'claude', host: null, env: { CLAUDE_CONFIG_DIR: '$HOME/.claude-cc1' }, args: [] }]

function props(over: Partial<AddIdentityDialogProps> = {}): AddIdentityDialogProps & { created: NewIdentityInput[]; logins: string[] } {
  const created: NewIdentityInput[] = []
  const logins: string[] = []
  return {
    open: true,
    hosts: LOCAL,
    existing: EXISTING,
    created,
    logins,
    onCreate: async (input) => {
      created.push(input)
      return true
    },
    onLogin: async (h, n) => {
      logins.push(`${h}/${n}`)
      return true
    },
    loggedIn: () => null,
    refreshStatus: async () => {},
    onClose: () => {},
    pollMs: 5,
    maxChecks: 3,
    ...over,
  }
}

const nameInput = () => document.querySelector<HTMLInputElement>(`.add-identity-field input[type=text]`)!
const submitBtn = () => [...document.querySelectorAll<HTMLButtonElement>('.add-identity-actions button')].find((b) => b.type === 'submit')!
const text = () => document.querySelector('.add-identity-dialog')?.textContent ?? ''

test('只有本機：顯示「建立於本機」，沒有主機選單', async () => {
  await mount(<AddIdentityDialog {...props()} />)
  assert.ok(text().includes('建立於本機'))
  assert.equal(document.querySelector('.add-identity-field select'), null)
})

test('輸入名稱：env 跟著填建議目錄，摘要顯示實際目錄', async () => {
  await mount(<AddIdentityDialog {...props()} />)
  await typeInto(nameInput(), 'cc2')
  const env = document.querySelector<HTMLTextAreaElement>('.add-identity-advanced textarea')!
  assert.equal(env.value, 'CLAUDE_CONFIG_DIR=$HOME/.claude-cc2')
  assert.ok(text().includes('$HOME/.claude-cc2'), '摘要的實際目錄')
  assert.equal(submitBtn().disabled, false)
})

test('同名的身份：擋住送出，並說明已存在', async () => {
  await mount(<AddIdentityDialog {...props()} />)
  await typeInto(nameInput(), 'cc1')
  assert.equal(submitBtn().disabled, true)
  assert.ok(text().includes('已經有名為 cc1'))
})

test('目錄與既有身份共用：顯示警告，但仍可送出', async () => {
  await mount(<AddIdentityDialog {...props()} />)
  await typeInto(nameInput(), 'cc1x')
  const env = document.querySelector<HTMLTextAreaElement>('.add-identity-advanced textarea')!
  await typeInto(env, 'CLAUDE_CONFIG_DIR=~/.claude-cc1/')
  assert.ok(text().includes('共用同一份登入'), '指到 cc1 的目錄要警告')
  assert.equal(submitBtn().disabled, false)
})

test('自訂過 env 後切 CLI：問保留或改用建議；改用建議會換成新 CLI 的目錄變數', async () => {
  await mount(<AddIdentityDialog {...props()} />)
  await typeInto(nameInput(), 'work')
  const env = document.querySelector<HTMLTextAreaElement>('.add-identity-advanced textarea')!
  await typeInto(env, 'CLAUDE_CONFIG_DIR=/srv/mine')
  const codex = document.querySelector<HTMLInputElement>('input[value=codex]')!
  await click(codex)
  assert.ok(text().includes('自訂值'), '切 CLI 時自訂值要提示')
  const use = [...document.querySelectorAll('button')].find((b) => b.textContent === '改用 codex 建議')!
  await click(use)
  assert.equal(env.value, 'CODEX_HOME=$HOME/.codex-work')
})

test('送出：建立（不帶 host＝本機）→ 開登入 → 輪詢到已登入', async () => {
  let state: boolean | null = false
  const p = props({
    loggedIn: () => state,
    refreshStatus: async () => {
      state = true
    },
  })
  await mount(<AddIdentityDialog {...p} />)
  await typeInto(nameInput(), 'cc3')
  await click(submitBtn())
  await until(() => text().includes('已登入'), '登入完成後顯示已登入')
  assert.deepEqual(p.created, [{ name: 'cc3', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '$HOME/.claude-cc3' } }])
  assert.deepEqual(p.logins, ['local/cc3'])
})

test('登入一直沒完成：達上限後顯示失敗與「重新檢查」，對話框不關', async () => {
  let closed = false
  await mount(<AddIdentityDialog {...props({ onClose: () => (closed = true) })} />)
  await typeInto(nameInput(), 'cc4')
  await click(submitBtn())
  await until(() => text().includes('還沒確認到登入'), '達上限後顯示失敗')
  assert.equal(closed, false)
  assert.ok([...document.querySelectorAll('button')].some((b) => b.textContent === '重新檢查'))
})
