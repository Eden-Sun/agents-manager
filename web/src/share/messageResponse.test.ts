import test from 'node:test'
import assert from 'node:assert/strict'
import { decideMessageResponse, type MessageResponseInput, type MessageResponseDecision } from './messageResponse'

const cases: { name: string; input: MessageResponseInput; expected: MessageResponseDecision }[] = [
  {
    name: '較舊成功低於已採用世代時丟棄',
    input: { requestGen: 2, latestIssuedGen: 3, kind: 'success', appliedGen: 3, settledGen: 3, state: 'active' },
    expected: 'discard',
  },
  {
    name: '成功與目前已採用世代相同時可採用',
    input: { requestGen: 3, latestIssuedGen: 3, kind: 'success', appliedGen: 3, settledGen: 3, state: 'active' },
    expected: 'apply',
  },
  {
    name: '尚未發出的世代不能被採用',
    input: { requestGen: 4, latestIssuedGen: 3, kind: 'success', appliedGen: 0, settledGen: 0, state: 'active' },
    expected: 'discard',
  },
  {
    name: '慢於輪詢間隔的第一趟成功仍可採用',
    input: { requestGen: 1, latestIssuedGen: 2, kind: 'success', appliedGen: 0, settledGen: 0, state: 'active' },
    expected: 'apply',
  },
  {
    name: '較新的成功到達後，慢於輪詢間隔的舊成功丟棄',
    input: { requestGen: 1, latestIssuedGen: 2, kind: 'success', appliedGen: 2, settledGen: 2, state: 'active' },
    expected: 'discard',
  },
  {
    name: '輪詢第 1 趟慢回 404 時，即使第 3 趟已發出仍判定失效',
    input: { requestGen: 1, latestIssuedGen: 3, kind: 'not_found', appliedGen: 0, settledGen: 0, state: 'active' },
    expected: 'expire',
  },
  {
    name: '輪詢第 2 趟慢回成功時，第 3 趟尚未有結果仍可採用',
    input: { requestGen: 2, latestIssuedGen: 3, kind: 'success', appliedGen: 1, settledGen: 1, state: 'active' },
    expected: 'apply',
  },
  {
    name: '輪詢第 3 趟成功採用後，第 1 趟晚到的失敗不覆蓋它',
    input: { requestGen: 1, latestIssuedGen: 3, kind: 'failure', appliedGen: 3, settledGen: 3, state: 'active' },
    expected: 'discard',
  },
  {
    name: '較新請求仍在路上時，較舊的網路失敗仍可採用',
    input: { requestGen: 1, latestIssuedGen: 2, kind: 'failure', appliedGen: 0, settledGen: 0, state: 'active' },
    expected: 'apply',
  },
  {
    name: '失敗與已裁決世代相同時可採用',
    input: { requestGen: 2, latestIssuedGen: 3, kind: 'failure', appliedGen: 1, settledGen: 2, state: 'active' },
    expected: 'apply',
  },
  {
    name: '較新的結果已處理後，較舊的網路失敗丟棄',
    input: { requestGen: 1, latestIssuedGen: 2, kind: 'failure', appliedGen: 2, settledGen: 2, state: 'active' },
    expected: 'discard',
  },
  {
    name: '較新的成功已採用後，慢到的輪詢失敗丟棄',
    input: { requestGen: 2, latestIssuedGen: 3, kind: 'failure', appliedGen: 3, settledGen: 3, state: 'active' },
    expected: 'discard',
  },
  {
    name: '較新請求仍在路上時，較舊趟回來的 404 仍轉成失效',
    input: { requestGen: 1, latestIssuedGen: 2, kind: 'not_found', appliedGen: 0, settledGen: 0, state: 'active' },
    expected: 'expire',
  },
  {
    name: '最新一趟 404 轉成失效',
    input: { requestGen: 2, latestIssuedGen: 2, kind: 'not_found', appliedGen: 1, settledGen: 1, state: 'active' },
    expected: 'expire',
  },
  {
    name: '已失效後再收到 404 保持終局並丟棄',
    input: { requestGen: 3, latestIssuedGen: 3, kind: 'not_found', appliedGen: 2, settledGen: 3, state: 'gone' },
    expected: 'discard',
  },
  {
    name: '失效後晚到的舊成功不能救回分享頁',
    input: { requestGen: 1, latestIssuedGen: 2, kind: 'success', appliedGen: 0, settledGen: 2, state: 'gone' },
    expected: 'discard',
  },
  {
    name: '分享頁已失效後其他失敗也丟棄',
    input: { requestGen: 2, latestIssuedGen: 2, kind: 'failure', appliedGen: 0, settledGen: 2, state: 'gone' },
    expected: 'discard',
  },
]

for (const row of cases) {
  test(row.name, () => {
    assert.equal(decideMessageResponse(row.input), row.expected)
  })
}
