/** 主機一致性備份列舉與破壞性還原確認（#719 A）。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, keydown, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { BaselineLine } from './HostsPanel'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 10_000 }, fn)

const backupId = '20261024T120000Z-0f8b2c1e-3a4d-4b5c-8d6e-7f8091a2b3c4'
const backupList = {
  skipped: 2,
  backups: [
    {
      backup_id: backupId,
      created_at: '2026-10-24T12:00:00Z',
      files: [
        { file: 'codex/config.toml', restore: 'overwrite' },
        { file: 'claude-default/settings.json', restore: 'delete' },
      ],
    },
  ],
}

it('sync=off 且 baseline 未知時仍可列舉，顯示略過數與安全檔名', async () => {
  const originalFetch = globalThis.fetch
  let listCalls = 0
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = typeof input === 'string' ? input : input.toString()
    assert.match(url, /\/api\/hosts\/worker-1\/baseline\/backups$/)
    listCalls++
    return new Response(JSON.stringify(backupList), { status: 200, headers: { 'Content-Type': 'application/json' } })
  }) as unknown as typeof fetch

  try {
    await mount(<BaselineLine host="worker-1" baseline={null} syncMode="off" />)
    const listButton = document.querySelector<HTMLButtonElement>('.host-baseline-restore-open')!
    assert.ok(listButton)
    assert.equal(listCalls, 0, '不在 host_baseline_sync 關閉時自動查詢')
    await click(listButton)
    assert.equal(listCalls, 1)
    assert.match(document.body.textContent ?? '', /有 2 份備份無法讀取/)
    assert.match(document.body.textContent ?? '', /2026-10-24T12:00:00Z/)
    assert.match(document.body.textContent ?? '', /codex\/config\.toml/)
    assert.match(document.body.textContent ?? '', /claude-default\/settings\.json/)
  } finally {
    globalThis.fetch = originalFetch
  }
})

it('還原確認列主機、時間、覆寫與刪除檔，預設 focus 在取消，Enter 不送出；等待時停用確認', async () => {
  const originalFetch = globalThis.fetch
  let restoreCalls = 0
  let resolveRestore: ((response: Response) => void) | null = null
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = typeof input === 'string' ? input : input.toString()
    if (url.endsWith('/baseline/backups')) {
      return new Response(JSON.stringify({ ...backupList, skipped: 0 }), { status: 200, headers: { 'Content-Type': 'application/json' } })
    }
    if (url.endsWith('/baseline/restore')) {
      restoreCalls++
      return await new Promise<Response>((resolve) => { resolveRestore = resolve })
    }
    return new Response('{}', { status: 404 })
  }) as unknown as typeof fetch

  try {
    await mount(<BaselineLine host="worker-1" baseline={null} syncMode="off" />)
    await click(document.querySelector<HTMLButtonElement>('.host-baseline-restore-open')!)
    await click([...document.querySelectorAll<HTMLButtonElement>('.host-baseline-restore-entry button')][0])

    let dialog = document.querySelector('[role=alertdialog]')!
    assert.match(dialog.textContent ?? '', /主機：worker-1/)
    assert.match(dialog.textContent ?? '', /備份時間：2026-10-24T12:00:00Z/)
    assert.match(dialog.textContent ?? '', /會覆寫的檔案：[\s\S]*codex\/config\.toml/)
    assert.match(dialog.textContent ?? '', /會刪除的檔案（備份時不存在）：[\s\S]*claude-default\/settings\.json/)
    assert.match(dialog.textContent ?? '', /還原前不會另外備份目前狀態/)

    const confirmButton = [...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((button) => button.textContent?.includes('確認還原'))!
    const cancelButton = [...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((button) => button.textContent?.includes('取消'))!
    assert.equal(document.activeElement, cancelButton)
    assert.notEqual(document.activeElement, confirmButton)
    await keydown(document.activeElement!, 'Enter')
    assert.equal(restoreCalls, 0, 'Enter cannot submit restore')

    await click([...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((button) => button.textContent?.includes('取消'))!)
    await click([...document.querySelectorAll<HTMLButtonElement>('.host-baseline-restore-entry button')][0])
    dialog = document.querySelector('[role=alertdialog]')!
    const liveConfirm = [...dialog.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].find((button) => button.textContent?.includes('確認還原'))!
    await click(liveConfirm)
    assert.equal(restoreCalls, 1)
    assert.equal(liveConfirm.disabled, true, 'confirm is disabled while the API is pending')

    await act(async () => {
      resolveRestore!(new Response(JSON.stringify({
        host: 'worker-1',
        backup_id: backupId,
        files: [
          { file: 'codex/config.toml', result: 'restored' },
          { file: 'claude-default/settings.json', result: 'deleted' },
        ],
      }), { status: 200, headers: { 'Content-Type': 'application/json' } }))
      await new Promise((resolve) => setTimeout(resolve, 0))
    })
    assert.equal(document.querySelector('[role=alertdialog]'), null)
    assert.match(document.body.textContent ?? '', /還原結果：20261024T120000Z/)
    assert.match(document.body.textContent ?? '', /codex\/config\.toml：已還原/)
    assert.match(document.body.textContent ?? '', /claude-default\/settings\.json：已刪除/)
  } finally {
    globalThis.fetch = originalFetch
  }
})

it('API 錯誤只顯示白名單錯誤碼與固定文案，不顯示主機回傳內容', async () => {
  const originalFetch = globalThis.fetch
  globalThis.fetch = (async () => new Response(JSON.stringify({
    error: 'upstream',
    reason: 'secret-host-error',
    message: 'PRIVATE_CONFIG_CONTENT /home/user/.ssh/id_rsa',
  }), { status: 500, headers: { 'Content-Type': 'application/json' } })) as unknown as typeof fetch
  try {
    await mount(<BaselineLine host="worker-1" baseline={null} syncMode="off" />)
    await click(document.querySelector<HTMLButtonElement>('.host-baseline-restore-open')!)
    const error = document.querySelector('[role=alert]')!
    assert.match(error.textContent ?? '', /operation_failed/)
    assert.match(error.textContent ?? '', /操作失敗/)
    assert.doesNotMatch(document.body.textContent ?? '', /PRIVATE_CONFIG_CONTENT|id_rsa|secret-host-error/)
  } finally {
    globalThis.fetch = originalFetch
  }
})
