/**
 * 互動測試的跑道：把元件真的掛進 happy-dom，發鍵盤／滑鼠事件、斷言行為（不是掃原始碼）。
 *
 * 用法：測試檔**第一個** import 這支（它在載入時就註冊全域 DOM），再 import 要測的元件；結尾 `after(teardownDom)`。
 * 事件都在 `act` 裡發，React 的更新照真的流程跑完才回來。
 */
import { GlobalRegistrator } from '@happy-dom/global-registrator'
import type { ReactElement } from 'react'

GlobalRegistrator.register({ url: 'http://localhost:5173' })
// happy-dom 換掉了全域 fetch：元件掛上去時會打的 API 一律回空物件，不碰網路。
globalThis.fetch = (async () => new Response('{}', { status: 200 })) as typeof fetch
;(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true

const React = await import('react')
const { createRoot } = await import('react-dom/client')

const mounted: Array<{ root: ReturnType<typeof createRoot>; host: HTMLElement }> = []

export const act = React.act

/** 掛進 body，回容器。 */
export async function mount(element: ReactElement): Promise<HTMLElement> {
  const host = document.createElement('div')
  document.body.appendChild(host)
  const root = createRoot(host)
  await act(async () => {
    root.render(element)
  })
  mounted.push({ root, host })
  return host
}

/** 每個測試結束時呼叫：卸掉所有掛上去的樹、清空 body，下一個測試從乾淨的 DOM 開始。 */
export async function unmountAll(): Promise<void> {
  for (const { root, host } of mounted.splice(0)) {
    await act(async () => root.unmount())
    host.remove()
  }
  document.body.innerHTML = ''
}

/** 一次滑鼠點擊：mousedown → mouseup → click，都會冒泡。 */
export async function click(el: Element): Promise<void> {
  await act(async () => {
    for (const type of ['mousedown', 'mouseup', 'click']) {
      el.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, button: 0 }))
    }
  })
}

/** 在 `el` 上發一個 keydown，回事件（看 `defaultPrevented`）。 */
export async function keydown(el: Element, key: string, init: KeyboardEventInit = {}): Promise<KeyboardEvent> {
  const event = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true, ...init })
  await act(async () => {
    el.dispatchEvent(event)
  })
  return event
}

/** 照瀏覽器的預設：點到非可聚焦的內容，焦點移到最近的可聚焦祖先（沒有就掉到 body）。 */
export function focusLikeBrowserClick(el: Element): void {
  const target = el.closest<HTMLElement>('button, input, select, textarea, a[href], [tabindex]')
  if (target) target.focus()
  else (document.activeElement as HTMLElement | null)?.blur()
}

export function teardownDom(): void {
  void GlobalRegistrator.unregister()
}
