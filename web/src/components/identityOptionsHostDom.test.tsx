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
