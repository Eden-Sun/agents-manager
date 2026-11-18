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
  await mount(<ComposerDraftBar botId="b1" text="保留的訊息" attachments={[]} onSent={() => { throw new Error('unexpected') }} onEdit={() => {}} />)
  const bar = document.querySelector<HTMLElement>('.composer-draft')!
  assert.match(bar.textContent ?? '', /讀不到終端內容.*目前訊息保留/)
  const buttons = [...bar.querySelectorAll('button')]
  assert.deepEqual(buttons.map(b => b.textContent), ['前往終端', '取消'])
  await act(async () => buttons[1].click())
  assert.equal(document.querySelector('.composer-draft'), null)
  assert.equal(sends, 0)
})

test('讀不到草稿時「前往終端」切到終端分頁，不清草稿', async () => {
  useStore.setState({
    rightTab: 'chat',
    composerDrafts: { b1: { draft: '', token: '', truncated: false, actions: [] } },
  } as never)
  await mount(<ComposerDraftBar botId="b1" text="" attachments={[]} onSent={() => {}} onEdit={() => {}} />)
  const buttons = [...document.querySelectorAll<HTMLElement>('.composer-draft button')]
  assert.deepEqual(buttons.map(b => b.textContent), ['前往終端', '取消'])
  await act(async () => buttons[0].click())
  assert.equal(useStore.getState().rightTab, 'terminal')
  assert.ok(useStore.getState().composerDrafts.b1, 'composer draft must stay')
  assert.ok(document.querySelector('.composer-draft'), 'notice must stay')
})

test('畫面是對話框（codex hook 審核框）：只講原因與去終端，不給「送出框裡那段」等草稿動作', async () => {
  let sends = 0
  useStore.setState({
    rightTab: 'chat',
    composerDrafts: { b1: { draft: '', token: '', truncated: false, actions: [], dialog: 'Codex 停在「Hooks need review」審核框，重新啟動這顆 bot 即可', runId: 'run-1' } },
    sendPrompt: async () => { sends++; return false },
  } as never)
  await mount(<ComposerDraftBar botId="b1" text="我的訊息" attachments={[]} onSent={() => { throw new Error('unexpected') }} onEdit={() => { throw new Error('unexpected') }} />)
  const bar = document.querySelector<HTMLElement>('.composer-draft')!
  assert.match(bar.textContent ?? '', /停在對話框/)
  assert.match(bar.textContent ?? '', /重新啟動這顆 bot 即可/)
  const buttons = [...bar.querySelectorAll('button')]
  assert.deepEqual(buttons.map(b => b.textContent), ['前往終端', '取消'])
  assert.doesNotMatch(bar.textContent ?? '', /送出框裡那段|清掉再送我這則|編輯後送出/)
  await act(async () => buttons[0].click())
  assert.equal(useStore.getState().rightTab, 'terminal')
  assert.equal(sends, 0)
})

test('run 換了（bot 重啟）：別的 run 留下的草稿列自動收掉，同一個 run 的留著', async () => {
  resetStoreForTest()
  const run = (id: string) => ({ id, state: 'running', agent_status: 'idle' })
  useStore.setState({
    runs: { b1: run('run-1'), b2: run('run-9') },
    composerDrafts: {
      b1: { draft: '1. Review hooks\n2. Trust all and continue', token: 'tok', truncated: false, actions: ['submit', 'clear'], runId: 'run-1' },
      b2: { draft: '還在的草稿', token: 'tok2', truncated: false, actions: ['submit', 'clear'], runId: 'run-9' },
    },
  } as never)
  await mount(<ComposerDraftBar botId="b1" text="" attachments={[]} onSent={() => {}} onEdit={() => {}} />)
  assert.ok(document.querySelector('.composer-draft'), '同一個 run：還在')
  await act(async () => { useStore.setState({ runs: { b1: run('run-2'), b2: run('run-9') } } as never) })
  assert.equal(document.querySelector('.composer-draft'), null, 'run 換了：草稿列與送出按鈕消失')
  assert.equal(useStore.getState().composerDrafts.b1, undefined)
  assert.ok(useStore.getState().composerDrafts.b2, '別顆 bot 的不受影響')
})
