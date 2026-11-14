/**
 * ApiModelConflictDialog：換模型後舊對話接續不了時的確認框。兩個選項（換回原模型、清除對話）各自呼叫
 * store 的 `resolveApiModelConflict`；沒有衝突時不顯示。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { ApiModelConflictDialog } from './ApiModelConflictDialog'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  useStore.setState({ apiModelConflict: null })
})
after(async () => {
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

function buttonWith(text: RegExp): HTMLButtonElement {
  const b = Array.from(document.querySelectorAll('button')).find((el) => text.test(el.textContent ?? ''))
  if (!b) throw new Error(`no button matching ${text}`)
  return b as HTMLButtonElement
}

it('沒有衝突時不顯示框', async () => {
  await mount(<ApiModelConflictDialog />)
  assert.equal(document.body.textContent?.includes('模型換了'), false)
})

it('顯示舊模型與新模型，兩個選項各自送出對應的動作', async () => {
  const calls: string[] = []
  useStore.setState({
    apiModelConflict: { botId: 'b1', checkpointModel: 'claude-opus-5-5', currentModel: 'claude-fable-5-2' },
    resolveApiModelConflict: async (action: 'revert' | 'clear') => {
      calls.push(action)
    },
  })
  await mount(<ApiModelConflictDialog />)
  const text = document.body.textContent ?? ''
  assert.ok(text.includes('claude-opus-5-5') && text.includes('claude-fable-5-2'), text)
  await click(buttonWith(/換回 claude-opus-5-5/))
  // 確認框有連點防護（800ms）：第二下要等過窗口才算數。
  await new Promise((r) => setTimeout(r, 900))
  await click(buttonWith(/清除對話/))
  assert.deepEqual(calls, ['revert', 'clear'])
})
