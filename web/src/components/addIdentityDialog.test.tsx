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

test('等待登入：輪詢時先呼叫伺服器重新檢查（不是只讀快取），檢查到已登入就完成', async () => {
  const rechecks: string[] = []
  let state: boolean | null = null
  const p = props({
    loggedIn: () => state,
    recheck: async (h, n) => {
      rechecks.push(`${h}/${n}`)
      state = true
    },
  })
  await mount(<AddIdentityDialog {...p} />)
  await typeInto(nameInput(), 'cc5')
  await click(submitBtn())
  await until(() => text().includes('已登入'), '重新檢查到已登入後顯示已登入')
  assert.deepEqual(rechecks, ['local/cc5'])
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

const CONNECTED_REMOTES = [
  { name: 'local', label: '本機', connected: true },
  { name: 'm4p', label: 'm4p', connected: true },
  { name: 'lab', label: 'lab', connected: false },
]
const alsoToggle = () => document.querySelector<HTMLInputElement>('.add-identity-also-toggle input')
const alsoList = () => document.querySelector('.add-identity-also-list')

test('其他已連線主機：checkbox 預設不勾，離線主機不列出', async () => {
  const p = props({ hosts: CONNECTED_REMOTES, initialHost: 'local' })
  await mount(<AddIdentityDialog {...p} />)
  assert.ok(alsoToggle(), '有其他已連線主機就顯示')
  assert.equal(alsoToggle()!.checked, false, '預設不勾')
  assert.equal(alsoToggle()!.closest('label')!.textContent!.includes('本機'), false, '主機清單排除目前主機')
  assert.ok(text().includes('同時在其他已連線主機建立（m4p）'))
  assert.ok(!alsoToggle()!.closest('label')!.textContent!.includes('lab'), '離線主機不列出')
  await typeInto(nameInput(), 'work')
  await click(submitBtn())
  await until(() => p.created.length === 1, '只建立主目標')
  assert.equal(p.created[0].host, undefined, '本機主目標照原本的輸入')
})

test('勾選後：對每台其他已連線主機各建立一次，完成後列出各自的登入按鈕', async () => {
  const remotes = [
    { name: 'local', label: '本機', connected: true },
    { name: 'm4p', label: 'm4p', connected: true },
    { name: 'mini', label: 'mini', connected: true },
  ]
  const p = props({ hosts: remotes, initialHost: 'mini' })
  await mount(<AddIdentityDialog {...p} />)
  await typeInto(nameInput(), 'work')
  await click(alsoToggle()!)
  await click(submitBtn())
  await until(() => alsoList() !== null, '完成後列出其他主機')
  assert.deepEqual(
    p.created.map((c) => c.host ?? 'local'),
    ['mini', 'local', 'm4p'],
    '主目標 mini 先建立，其他已連線主機各一次',
  )
  const items = [...alsoList()!.querySelectorAll('li')].map((li) => li.textContent ?? '')
  assert.equal(items.length, 2)
  const loginM4p = [...alsoList()!.querySelectorAll('button')].find((b) => b.closest('li')?.textContent?.includes('m4p'))!
  await click(loginM4p)
  assert.deepEqual(p.logins.slice(-1), ['m4p/work'])
})

test('其他主機已有同名身份：顯示略過提示，該台不建立，主機建立不被阻擋', async () => {
  const existing: Identity[] = [...EXISTING, { name: 'work', kind: 'claude', host: 'm4p', env: {}, args: [] }]
  const p = props({ hosts: CONNECTED_REMOTES, existing, initialHost: 'local' })
  await mount(<AddIdentityDialog {...p} />)
  await typeInto(nameInput(), 'work')
  assert.ok(text().includes('m4p 已有同名身份，將略過'))
  await click(alsoToggle()!)
  await click(submitBtn())
  await until(() => p.created.length >= 1 && p.logins.length >= 1, '主目標建立並進入登入')
  assert.deepEqual(p.created.map((c) => c.host ?? 'local'), ['local'], '同名的 m4p 略過')
  assert.equal(alsoList(), null, '沒有其他主機要登入')
})
