/** 譯文來源短名（使用者 2026-11-18：「UI 不是 by gpt 而是 by luna」）：OpenAI 取名稱、Claude 規則不變。 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { isLocalTranslation, translationModelName } from '../components/messageTranslationConstants'

test('OpenAI：gpt-<版本>-<名稱> 取名稱首字大寫', () => {
  assert.equal(translationModelName('gpt-6-luna'), 'Luna')
  assert.equal(translationModelName('gpt-6.2-sol'), 'Sol')
  assert.equal(translationModelName('gpt-10-astra'), 'Astra')
})

test('Claude 與本地轉換的規則不變', () => {
  assert.equal(translationModelName('claude-sonnet-5-5'), 'Sonnet')
  assert.equal(translationModelName('claude-haiku-5-5'), 'Haiku')
  assert.equal(translationModelName('fable-5-2'), 'Fable')
  assert.equal(translationModelName('opencc-s2twp'), 'OpenCC')
  assert.equal(isLocalTranslation('opencc-s2twp'), true)
  assert.equal(isLocalTranslation('gpt-6-luna'), false)
})
