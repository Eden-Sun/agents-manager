/**
 * Herdr 版本 payload 的契約測試。
 *
 * 這張卡唯一的價值是「不要讓人以為自己已經是最新的」，所以每個測試問的都是同一件事：
 * daemon 回了半殘、舊版或整個型別都不對的東西時，畫面會不會變成一句「已是最新」。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { emptyUpdates, RELEASES_URL, SOURCE_URL, toUpdates } from './herdrUpdates.ts'

test('沒有 latest.version 時不會有人被說成最新，而且標成舊資料', () => {
  const u = toUpdates({
    latest: { version: null, stale: true, error: 'HTTP 503' },
    hosts: [{ host: 'local', connected: true, server: { version: '0.8.2', standing: 'unknown' }, disk: {} }],
  })
  assert.equal(u.latest.version, null)
  assert.equal(u.latest.stale, true)
  assert.equal(u.latest.error, 'HTTP 503')
  assert.equal(u.hosts[0]?.server.standing, 'unknown')
  assert.deepEqual(u.behindHosts, [])
  assert.equal(u.unread, false, '什麼都不知道時不該跳未讀')
})

test('沒有版本號的那一邊一定是 unknown，就算 daemon 說它 latest', () => {
  const u = toUpdates({
    latest: { version: '0.9.0', stale: false },
    hosts: [{ host: 'box', server: { version: null, standing: 'latest' }, disk: { version: null, standing: 'latest' } }],
  })
  assert.equal(u.hosts[0]?.server.standing, 'unknown')
  assert.equal(u.hosts[0]?.disk.standing, 'unknown')
  assert.deepEqual(u.unknownHosts, ['box'], '問不到版本的主機要列出來，不併進「都最新」')
})

test('看不懂的 standing 收成 unknown，不是 latest', () => {
  for (const junk of ['up-to-date', '', 42, null, undefined, {}, true]) {
    const u = toUpdates({ hosts: [{ host: 'h', server: { version: '0.8.2', standing: junk }, disk: {} }] })
    assert.equal(u.hosts[0]?.server.standing, 'unknown', `${JSON.stringify(junk)} 不該被當成已知狀態`)
  }
})

test('stale 沒給就當舊資料（寧可多說一次「這是上次查到的」）', () => {
  assert.equal(toUpdates({ latest: { version: '0.9.0' } }).latest.stale, true)
  assert.equal(toUpdates({ latest: { version: '0.9.0', stale: false } }).latest.stale, false)
})

test('集合欄位不是陣列時不會 throw（.map 白屏的那個 bug）', () => {
  for (const junk of [{}, 'nope', 42, true, null, { length: 3 }]) {
    const u = toUpdates({ hosts: junk, notes: { sections: junk, missing_notes: junk } })
    assert.deepEqual(u.hosts, [])
    assert.deepEqual(u.notes.sections, [])
    assert.deepEqual(u.notes.missingNotes, [])
  }
  // 陣列裡的垃圾條目被濾掉，不是整包爆掉。
  const mixed = toUpdates({ hosts: [null, 'x', 42, {}, { host: '' }, { host: 'local', connected: true }] })
  assert.equal(mixed.hosts.length, 1)
  assert.equal(mixed.hosts[0]?.host, 'local')
})

test('落後的主機從每台自己的狀態算，不是信 daemon 給的總數', () => {
  const u = toUpdates({
    latest: { version: '0.9.0', stale: false },
    // daemon 說沒人落後，但每台的 standing 說有：以每台為準。
    behind_hosts: [],
    hosts: [
      { host: 'local', server: { version: '0.9.0', standing: 'latest' }, disk: { version: '0.9.0', standing: 'latest' } },
      { host: 'box', server: { version: '0.8.2', standing: 'behind' }, disk: { version: '0.8.2', standing: 'behind' } },
      { host: 'old', server: { version: '0.8.0', standing: 'latest' }, disk: { version: '0.9.0', standing: 'behind' } },
    ],
  })
  assert.deepEqual(u.behindHosts, ['box', 'old'], '任一邊落後就算，而且一台落後不會牽連別台')
})

test('磁碟比 server 新 = 待套用，只有一邊知道時不猜', () => {
  const u = toUpdates({
    hosts: [
      { host: 'a', server: { version: '0.8.2' }, disk: { version: '0.9.0' }, restart_pending: true },
      { host: 'b', server: { version: null }, disk: { version: '0.9.0' } },
    ],
  })
  assert.equal(u.hosts[0]?.restartPending, true)
  assert.equal(u.hosts[1]?.restartPending, false)
})

test('release notes 缺資料要留著訊息，不能被當成有內容', () => {
  const u = toUpdates({
    notes: {
      from: '0.8.3',
      to: '0.9.0',
      complete: false,
      gap: '官方清單裡沒有 0.8.3 這一版',
      missing_notes: ['0.8.5'],
      sections: [
        { version: '0.9.0', notes: '### Added' },
        { version: '0.8.5', notes: '' },
        { version: '', notes: 'no version' },
      ],
    },
  })
  assert.equal(u.notes.complete, false)
  assert.equal(u.notes.gap, '官方清單裡沒有 0.8.3 這一版')
  assert.deepEqual(u.notes.missingNotes, ['0.8.5'])
  assert.deepEqual(
    u.notes.sections.map((s) => [s.version, s.notes]),
    [
      ['0.9.0', '### Added'],
      ['0.8.5', null],
    ],
    '空字串的 notes 是「沒有」，沒有版本號的段落丟掉',
  )
})

test('端點不存在（舊 daemon）也是未知，不是最新', () => {
  const u = emptyUpdates('GET /herdr/updates failed (404)')
  assert.equal(u.latest.version, null)
  assert.equal(u.latest.stale, true)
  assert.equal(u.error, 'GET /herdr/updates failed (404)')
  assert.equal(u.unread, false)
  assert.equal(u.readOnly, true, '不知道就不要給人以為可以按什麼')
  assert.equal(u.latest.sourceUrl, SOURCE_URL)
  assert.equal(u.latest.releasesUrl, RELEASES_URL)
})

test('官方連結壞掉時退回寫死的官方網址，不會變成 undefined', () => {
  const u = toUpdates({ latest: { version: '0.9.0', source_url: 42, releases_url: '' } })
  assert.equal(u.latest.sourceUrl, SOURCE_URL)
  assert.equal(u.latest.releasesUrl, RELEASES_URL)
})

test('整包不是物件也撐得住', () => {
  for (const junk of [null, undefined, 'nope', 42, []]) {
    const u = toUpdates(junk)
    assert.equal(u.latest.version, null)
    assert.deepEqual(u.hosts, [])
    assert.equal(u.unread, false)
  }
})
