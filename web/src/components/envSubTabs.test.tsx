import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { act, click, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { EnvSubTabs, type EnvSubTabItem } from './EnvSubTabs'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

test('EnvSubTabs: renders tablist and tabs with correct accessibility attributes', async () => {
  function TestHost() {
    const [tab, setTab] = useState<'form' | 'book'>('form')
    const items: EnvSubTabItem<'form' | 'book'>[] = [
      { id: 'form', label: '設定', panelId: 'jev-form', content: <div>設定內容</div> },
      { id: 'book', label: '帳本', panelId: 'jev-book', content: <div>帳本內容</div> },
    ]
    return <EnvSubTabs<'form' | 'book'> ariaLabel="Jev 建議分類" value={tab} onChange={setTab} items={items} />
  }

  const container = await mount(<TestHost />)

  const tablist = container.querySelector('[role="tablist"]')
  assert.ok(tablist, 'tablist should exist')
  assert.equal(tablist.getAttribute('aria-label'), 'Jev 建議分類')

  const tabs = container.querySelectorAll<HTMLButtonElement>('[role="tab"]')
  assert.equal(tabs.length, 2)
  assert.equal(tabs[0].getAttribute('aria-selected'), 'true')
  assert.equal(tabs[0].getAttribute('aria-controls'), 'jev-form')
  assert.equal(tabs[1].getAttribute('aria-selected'), 'false')
  assert.equal(tabs[1].getAttribute('aria-controls'), 'jev-book')

  const panels = container.querySelectorAll('[role="tabpanel"]')
  assert.equal(panels.length, 2)
  assert.equal(panels[0].id, 'jev-form')
  assert.equal(panels[0].getAttribute('hidden'), null)
  assert.equal(panels[1].id, 'jev-book')
  assert.notEqual(panels[1].getAttribute('hidden'), null)

  // Click tab 1
  await click(tabs[1])

  assert.equal(tabs[0].getAttribute('aria-selected'), 'false')
  assert.equal(tabs[1].getAttribute('aria-selected'), 'true')
  assert.notEqual(panels[0].getAttribute('hidden'), null)
  assert.equal(panels[1].getAttribute('hidden'), null)

  // Keyboard navigation: ArrowLeft -> select 0
  await act(async () => {
    tabs[1].dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowLeft', bubbles: true }))
  })
  assert.equal(tabs[0].getAttribute('aria-selected'), 'true')
  assert.equal(panels[0].getAttribute('hidden'), null)

  // Keyboard navigation: ArrowRight -> select 1
  await act(async () => {
    tabs[0].dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }))
  })
  assert.equal(tabs[1].getAttribute('aria-selected'), 'true')
  assert.equal(panels[1].getAttribute('hidden'), null)

  // Keyboard navigation: Home -> select 0
  await act(async () => {
    tabs[1].dispatchEvent(new KeyboardEvent('keydown', { key: 'Home', bubbles: true }))
  })
  assert.equal(tabs[0].getAttribute('aria-selected'), 'true')

  // Keyboard navigation: End -> select 1
  await act(async () => {
    tabs[0].dispatchEvent(new KeyboardEvent('keydown', { key: 'End', bubbles: true }))
  })
  assert.equal(tabs[1].getAttribute('aria-selected'), 'true')
})
