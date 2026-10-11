/**
 * #1238：Markdown 的 `[![badge](圖片網址)](連結網址)` 會把 `MarkdownImage` 包在 `<a>` 裡。
 * 「載入圖片」按鈕的 click 冒泡到外層 `<a>`，瀏覽器會照樣開外層連結；按鈕要自己擋掉預設動作，只負責抓這張圖。
 */
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

/** 永遠不回來的 fetch：只記錄被叫的網址，讓測試停在「載入中」。 */
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
  // 瀏覽器在派送結束後才執行連結的預設動作，所以這裡看 defaultPrevented 就是判斷依據。
  // happy-dom 會在冒泡經過 `<a>` 時就先導覽，測試輸出裡可能還會看到對 link.example 的 ECONNREFUSED，那是它的雜訊。
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
