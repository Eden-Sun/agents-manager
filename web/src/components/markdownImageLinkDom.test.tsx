import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { MarkdownImage } from './MarkdownImage'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  await teardownDom()
})

const loadButton = () =>
  [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent === '載入圖片') ?? null

function neverFetch(): string[] {
  const calls: string[] = []
  globalThis.fetch = ((input: RequestInfo | URL) => {
    calls.push(String(input))
    return new Promise(() => {})
  }) as unknown as typeof fetch
  return calls
}

test('連結裡的載入圖片不會把連結開出去', async () => {
  const calls = neverFetch()
  await mount(
    <a href="https://link.example/" target="_blank" rel="noopener noreferrer">
      <MarkdownImage botId="b1" src="https://img.example/a.png" alt="badge" />
    </a>,
  )
  await until(() => loadButton() !== null, '載入圖片按鈕出現')
  const event = new MouseEvent('click', { bubbles: true, cancelable: true })
  await act(() => {
    loadButton()!.dispatchEvent(event)
  })
  assert.equal(event.defaultPrevented, true, '外層連結的預設導覽要被取消')
  assert.equal(calls.length, 1, 'fetch 只被叫一次')
  assert.equal(calls[0], 'https://img.example/a.png')
  assert.equal(loadButton(), null, '按鈕換成「載入中…」（已停用）')
  assert.ok(document.body.textContent?.includes('載入中…'))
})

test('不在連結裡時行為不變', async () => {
  const calls = neverFetch()
  await mount(<MarkdownImage botId="b1" src="https://img.example/a.png" alt="badge" />)
  await until(() => loadButton() !== null, '載入圖片按鈕出現')
  const event = new MouseEvent('click', { bubbles: true, cancelable: true })
  await act(() => {
    loadButton()!.dispatchEvent(event)
  })
  assert.equal(calls.length, 1, 'fetch 只被叫一次')
  assert.equal(calls[0], 'https://img.example/a.png')
})
