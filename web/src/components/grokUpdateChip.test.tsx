import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { GrokUpdateChip } from './GrokUpdateChip.tsx'
import { grokChipLabel } from '../lib/grokUpdate.ts'

test('grok 都追上了（沒有快照）就不畫', () => {
  assert.equal(renderToStaticMarkup(<GrokUpdateChip />), '')
})

test('tooltip 寫新版、落後主機與指令；只提示、不說「安裝並重啟」（#761）', () => {
  const t = grokChipLabel({ target: '1.0.47', hosts: [{ host: 'local', from: '1.0.46' }, { host: 'm4p', from: null }], command: 'grok update' })
  assert.match(t, /grok 有新版 1\.0\.47/)
  assert.match(t, /local：1\.0\.46；m4p：讀不到版本/)
  assert.match(t, /`grok update`/)
  assert.doesNotMatch(t, /安裝並重啟/)
})
