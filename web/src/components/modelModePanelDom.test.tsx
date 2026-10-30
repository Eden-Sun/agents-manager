import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, settle, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest } from '../store/store'
import { ModelModePanel } from './ModelModePanel'

// 這裡直接換掉全域 fetch：每條測試後放回，否則同一個 bun 行程（分片）後面的測試會吃到這份假回應。
const realFetch = globalThis.fetch
afterEach(async () => {
  globalThis.fetch = realFetch
  await unmountAll()
})
before(setupDom)
after(async () => {
  globalThis.fetch = realFetch
  resetStoreForTest()
  await teardownDom()
})

function stub(puts: string[]) {
  let mode = 'claude'
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    const path = String(input).replace(/^\/api/, '').split('?')[0]
    if (path === '/session') return new Response(JSON.stringify({ token: 't' }), { status: 200 })
    if (path === '/model-mode' && init?.method === 'PUT') {
      puts.push(String(init.body))
      mode = (JSON.parse(String(init.body)) as { mode: string }).mode
      return new Response(JSON.stringify({ mode, needs_restart_bots: true }), { status: 200 })
    }
    if (path === '/model-mode') {
      return new Response(
        JSON.stringify({
          mode,
          modes: [
            { id: 'claude', label: 'claude', description: 'DESC-A' },
            { id: 'claude_codex', label: 'claude主codex輔助模式', description: 'DESC-B' },
          ],
        }),
        { status: 200 },
      )
    }
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch
}

test('顯示 daemon 給的兩個模式與說明，預設選 claude；改選會 PUT 並提示重啟 bot', async () => {
  resetStoreForTest()
  const puts: string[] = []
  stub(puts)
  const root = await mount(<ModelModePanel />)
  await settle()
  const radios = [...root.querySelectorAll<HTMLInputElement>('input[type=radio]')]
  assert.equal(radios.length, 2)
  assert.equal(radios[0].checked, true)
  assert.match(root.textContent ?? '', /DESC-B/)
  radios[1].click()
  await settle()
  assert.equal(puts.length, 1)
  assert.deepEqual(JSON.parse(puts[0]), { mode: 'claude_codex' })
  assert.match(root.querySelector('.model-mode-note')?.textContent ?? '', /重啟/)
  assert.equal(root.querySelectorAll<HTMLInputElement>('input[type=radio]')[1].checked, true)
})

test('舊 daemon（404）不顯示這個區塊', async () => {
  resetStoreForTest()
  globalThis.fetch = (async (input: string) => {
    const path = String(input).replace(/^\/api/, '')
    if (path === '/session') return new Response(JSON.stringify({ token: 't' }), { status: 200 })
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch
  const root = await mount(<ModelModePanel />)
  await settle()
  assert.equal(root.querySelectorAll('input').length, 0)
})
