import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { shareFileStubs } from '../testing/shareFileStubs'
import { ShareApp } from './ShareApp'
import type { ShareClient } from './shareApi'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

test('字數照 Unicode 字元數：4001 個 emoji（UTF-16 長度 8002）daemon 收得下，網頁不擋、不說「太長了」', async () => {
  const sent: string[] = []
  const client: ShareClient = {
    async messages() {
      return { bot_name: 'b', status: 'idle', messages: [], has_more: false }
    },
    async send(text) {
      sent.push(text)
    },
    ...shareFileStubs,
    fileBlob: async () => new Blob([]),
    subscribe: () => () => {},
  }
  await mount(<ShareApp client={client} />)
  await settle(20)
  const text = '😀'.repeat(4001)
  await typeInto(document.querySelector('textarea')!, text)
  await settle(20)
  assert.doesNotMatch(document.body.textContent ?? '', /太長了/)
  const btn = document.querySelector<HTMLButtonElement>('.sh-send')!
  assert.equal(btn.disabled, false, '收得下就要能送')
  await click(btn)
  await settle(20)
  assert.deepEqual(sent, [text])
})
