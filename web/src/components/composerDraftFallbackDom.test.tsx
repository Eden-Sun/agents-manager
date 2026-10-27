import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { ComposerDraftBar } from './ComposerDraftBar'

before(setupDom)
afterEach(unmountAll)
after(async () => { resetStoreForTest(); await teardownDom() })

test('讀不到草稿時常駐提示，取消不會送出或清除', async () => {
  let sends = 0
  useStore.setState({
    composerDrafts: { b1: { draft: '', token: '', truncated: false, actions: [] } },
    sendPrompt: async () => { sends++; return false },
  } as never)
  await mount(<ComposerDraftBar botId="b1" text="保留的訊息" attachments={[]} onSent={() => { throw new Error('unexpected') }} />)
  const bar = document.querySelector<HTMLElement>('.composer-draft')!
  assert.match(bar.textContent ?? '', /讀不到終端內容.*目前訊息保留/)
  const buttons = [...bar.querySelectorAll('button')]
  assert.deepEqual(buttons.map(b => b.textContent), ['取消'])
  await act(async () => buttons[0].click())
  assert.equal(document.querySelector('.composer-draft'), null)
  assert.equal(sends, 0)
})
