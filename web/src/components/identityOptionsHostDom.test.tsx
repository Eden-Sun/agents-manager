import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { IdentityOptions } from './BotSettingsPanel'
import type { Host, Identity } from '../api/types'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const fakeHost: Host = {
  name: 'm4p',
  ssh: 'm4p@host',
  ssh_port: 22,
  herdr_session: 'test',
  remote_path: '',
  connected: true,
  error: null,
  baseline: null,
  identity_status: {},
} as unknown as Host

const identities: Identity[] = [
  { name: 'work', kind: 'claude', env: {}, args: [], host: null },
  { name: 'cc1', kind: 'claude', env: {}, args: [], host: 'm4p' },
]

test('本機：第一顆照舊', async () => {
  useStore.setState({
    identities,
    hosts: [fakeHost],
    disabledIdentities: [],
  })
  await mount(<IdentityOptions kind="claude" host="local" value="" onChange={() => {}} />)
  const buttons = document.querySelectorAll('[role="radiogroup"] button')
  assert.ok(buttons.length > 0)
  assert.equal(buttons[0].textContent?.trim(), '不指定身分（本機預設）')
})

test('遠端：寫那台主機', async () => {
  let picked = 'initial'
  useStore.setState({
    identities,
    hosts: [fakeHost],
    disabledIdentities: [],
  })
  await mount(<IdentityOptions kind="claude" host="m4p" value="" onChange={(v) => { picked = v }} />)
  const buttons = document.querySelectorAll('[role="radiogroup"] button')
  assert.ok(buttons.length > 0)
  assert.equal(buttons[0].textContent?.trim(), '不指定身分（m4p 的預設帳號）')

  await click(buttons[0])
  assert.equal(picked, '')
})

const localIdentities: Identity[] = [
  { name: 'work', kind: 'claude', env: {}, args: [], host: null },
  { name: 'cc1', kind: 'claude', env: {}, args: [], host: null },
]
const status = (name: string, logged_in: boolean | null) => ({ name, kind: 'claude', logged_in, reason: null, account: null, plan: null, source: 'config' }) as never

test('身份選項是 radio：選中的那顆 aria-checked=true，其餘 false', async () => {
  useStore.setState({
    identities: localIdentities,
    localIdentityStatus: { work: status('work', true), cc1: status('cc1', true) },
    disabledIdentities: [],
  })
  await mount(<IdentityOptions kind="claude" host="local" value="cc1" onChange={() => {}} />)
  const radios = [...document.querySelectorAll<HTMLButtonElement>('[role="radiogroup"][aria-label="身份"] [role="radio"]')]
  assert.equal(radios.length, localIdentities.length + 1, '「不指定身分」加上每個身份各一顆')
  const checked = radios.filter((b) => b.getAttribute('aria-checked') === 'true').map((b) => b.textContent?.trim())
  assert.deepEqual(checked, ['cc1'])
})

test('未選任何身份：「不指定身分」那顆是 aria-checked=true', async () => {
  useStore.setState({
    identities: localIdentities,
    localIdentityStatus: { work: status('work', true), cc1: status('cc1', true) },
    disabledIdentities: [],
  })
  await mount(<IdentityOptions kind="claude" host="local" value="" onChange={() => {}} />)
  const first = document.querySelector<HTMLButtonElement>('[role="radiogroup"][aria-label="身份"] [role="radio"]')
  assert.equal(first?.getAttribute('aria-checked'), 'true')
  assert.equal(first?.textContent?.trim(), '不指定身分（本機預設）')
})

test('未登入的身份：名稱帶狀態，已登入的沒有 aria-label', async () => {
  useStore.setState({
    identities: localIdentities,
    localIdentityStatus: { work: status('work', true), cc1: status('cc1', false) },
    disabledIdentities: [],
  })
  await mount(<IdentityOptions kind="claude" host="local" value="" onChange={() => {}} />)
  const byText = (t: string) => [...document.querySelectorAll<HTMLButtonElement>('[role="radio"]')].find((b) => b.textContent?.trim() === t)
  assert.equal(byText('cc1')?.getAttribute('aria-label'), 'cc1（未登入）')
  assert.equal(byText('work')?.hasAttribute('aria-label'), false)
})
