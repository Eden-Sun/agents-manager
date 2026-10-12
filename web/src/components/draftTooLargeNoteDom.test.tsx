/**
 * 草稿太大被 daemon 拒收（#1656）：輸入框上方要有一行說明，縮短、送成功（清掉提示）後消失；換一個輸入框的 key 不跟著顯示。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { DraftTooLargeNote } from './DraftTooLargeNote'
import { useDraftNotice } from '../store/draftNotice'
import { ApiError } from '../api/types'

before(setupDom)
afterEach(async () => {
  await unmountAll()
  await act(() => useDraftNotice.setState({ rejected: {} }))
})
after(teardownDom)

test('沒有被拒的草稿：不畫任何說明', async () => {
  await mount(<DraftTooLargeNote draftKey="bot:a" />)
  assert.equal(document.querySelector('.draft-too-large'), null)
})

test('被拒後顯示一行說明（帶 daemon 給的上限），clear 後消失，只對自己的 key 有效', async () => {
  await mount(<DraftTooLargeNote draftKey="bot:a" />)
  await act(() => useDraftNotice.getState().mark('bot:a', new ApiError(400, { error: 'draft_too_large', max_bytes: 262144 }, 'x')))
  const note = document.querySelector<HTMLElement>('.draft-too-large')
  assert.ok(note, '被拒的草稿要有說明')
  assert.equal(note.getAttribute('role'), 'status')
  assert.match(note.textContent ?? '', /這份草稿太長（上限 256 KB），沒有同步到其他裝置/)

  await act(() => useDraftNotice.getState().mark('group:p', new ApiError(413, {}, 'too large')))
  await act(() => useDraftNotice.getState().clear('bot:a'))
  assert.equal(document.querySelector('.draft-too-large'), null, '說明跟著草稿收掉')
})

test('別的 key 被拒，這個輸入框不顯示', async () => {
  await mount(<DraftTooLargeNote draftKey="bot:a" />)
  await act(() => useDraftNotice.getState().mark('group:p', new ApiError(400, { max_bytes: 262144 }, 'x')))
  assert.equal(document.querySelector('.draft-too-large'), null)
})
