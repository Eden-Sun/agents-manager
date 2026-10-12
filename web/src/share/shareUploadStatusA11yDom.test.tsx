/**
 * 分享頁上傳的狀態文字要念得出來：上傳中（第幾張）是 status，沒傳上去的錯誤是 alert。
 * 不能只是列表裡一段沒有 live 角色的文字，讀屏使用者選了檔案之後什麼都聽不到。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ShareApp } from './ShareApp'
import { mockShareClient } from './shareMock'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const TOKEN = 'demo_share_token_0123456789'

async function pick(client: ReturnType<typeof mockShareClient>, name: string) {
  await mount(<ShareApp client={client} />)
  await settle(300)
  const input = document.querySelector('input[type=file]') as HTMLInputElement
  await act(async () => {
    Object.defineProperty(input, 'files', { configurable: true, value: [new File(['x'], name, { type: 'text/plain' })] })
    input.dispatchEvent(new Event('change', { bubbles: true }))
  })
}

test('上傳失敗：「這張沒傳上去」是 role=alert，讀屏會念出來', async () => {
  const client = mockShareClient(TOKEN)
  client.upload = async () => {
    throw new Error('network down')
  }
  await pick(client, 'x.txt')
  await settle(50)
  await settle(50)
  const st = document.querySelector<HTMLElement>('.sh-pending li.err .sh-pending-st')!
  assert.match(st.textContent!, /這張沒傳上去/)
  assert.equal(st.getAttribute('role'), 'alert')
  await click(document.querySelector('.sh-retry')!)
})

test('上傳中：狀態文字是 role=status（進度換一張才念一次，不是每秒）', async () => {
  const client = mockShareClient(TOKEN)
  let release!: () => void
  const gate = new Promise<void>((r) => (release = r))
  client.upload = async (f: File) => {
    await gate
    return { id: `att-${f.name}`, name: f.name }
  }
  await pick(client, 'y.txt')
  await settle(20)
  const st = document.querySelector<HTMLElement>('.sh-pending li.up .sh-pending-st')!
  assert.match(st.textContent!, /上傳中/)
  assert.equal(st.getAttribute('role'), 'status')
  release()
  await settle(20)
})
