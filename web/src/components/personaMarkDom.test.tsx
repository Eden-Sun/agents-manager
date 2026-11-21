import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { PersonaMark } from './BotSettingsPanel'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/

test('人設超過 80 字：截斷後加「…」，截點落在 emoji 代理對中間時不留落單的代理字元', async () => {
  const persona = `${'a'.repeat(79)}😀bc`
  await mount(<PersonaMark persona={persona} />)
  const title = document.querySelector<HTMLElement>('.persona-mark')!.title
  assert.equal(title, `人設：${'a'.repeat(79)}😀…`)
  assert.doesNotMatch(title, LONE_SURROGATE, '不能留下落單的代理字元')
})

test('人設不超過 80 字：原文照寫、沒有「…」', async () => {
  await mount(<PersonaMark persona={'a'.repeat(80)} />)
  assert.equal(document.querySelector<HTMLElement>('.persona-mark')!.title, `人設：${'a'.repeat(80)}`)
})
