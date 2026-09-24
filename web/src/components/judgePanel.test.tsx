import test from 'node:test'
import assert from 'node:assert/strict'
import { renderToStaticMarkup } from 'react-dom/server'
import { JudgeLoadNotice, type JudgeLoad } from './JudgePanel.tsx'

const render = (state: JudgeLoad) => renderToStaticMarkup(<JudgeLoadNotice state={state} onRetry={() => {}} />)

/** issue #466：三種狀態要分得開，不能都長成「讀取中…」或都長成「需要更新 daemon」。 */
test('404／405：說的是 daemon 沒有這個功能', () => {
  const html = render({ kind: 'unsupported' })
  assert.match(html, /需要更新 daemon/)
  assert.doesNotMatch(html, /讀取中/)
  assert.doesNotMatch(html, /重試/)
})

test('讀取中：不提錯誤、也不提更新 daemon', () => {
  const html = render({ kind: 'loading' })
  assert.match(html, /讀取中/)
  assert.doesNotMatch(html, /需要更新 daemon/)
  assert.doesNotMatch(html, /讀不到/)
})

test('500 這類錯誤：顯示真正的訊息並給重試，不會講成 daemon 太舊，也不會卡在讀取中', () => {
  const html = render({ kind: 'error', message: 'judge settings unavailable (500)' })
  assert.match(html, /讀不到 Jev 設定/)
  assert.match(html, /judge settings unavailable \(500\)/)
  assert.match(html, /重試/)
  assert.doesNotMatch(html, /需要更新 daemon/)
  assert.doesNotMatch(html, /讀取中/)
})

test('連線層失敗：同樣是錯誤狀態而不是永遠的「讀取中…」', () => {
  const html = render({ kind: 'error', message: 'Failed to fetch' })
  assert.match(html, /讀不到 Jev 設定/)
  assert.match(html, /Failed to fetch/)
  assert.match(html, /重試/)
  assert.doesNotMatch(html, /讀取中/)
})
