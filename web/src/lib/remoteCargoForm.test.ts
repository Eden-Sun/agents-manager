import test from 'node:test'
import assert from 'node:assert/strict'
import type { RemoteCargoSettings } from '../api/types.ts'
import { remoteCargoUnsaved, toRemoteCargoInput, type RemoteCargoForm } from './remoteCargoForm.ts'

const saved: RemoteCargoSettings = {
  enabled: true,
  host: '192.168.1.46',
  user: 'ubuntu',
  ssh_port: 22,
  remote_root: '.cache/agents-manager/remote-cargo',
  cargo_jobs: 4,
  password_set: true,
}
const form = (over: Partial<RemoteCargoForm> = {}): RemoteCargoForm => ({
  enabled: true,
  host: '192.168.1.46',
  user: 'ubuntu',
  port: '22',
  root: '.cache/agents-manager/remote-cargo',
  jobs: '4',
  password: '',
  clearPassword: false,
  ...over,
})

/**
 * `POST /api/build/remote/test` 不收 body，測的是**已儲存**的設定；按鈕卻依表單裡的主機／帳號決定能不能按。
 * 表單有沒存的改動時測試連線會測到舊主機、報「✓ 可用」（真畫面：表單填 bogus-host.invalid，結果是舊主機的 cargo 版本）。
 */
test('表單跟已儲存的一樣：不用先存', () => {
  assert.equal(remoteCargoUnsaved(saved, form()), false)
  // 前後空白、port／jobs 打成同一個數字不算改動（儲存時送出去的值一樣）。
  assert.equal(remoteCargoUnsaved(saved, form({ host: ' 192.168.1.46 ', port: '022', jobs: '4 ' })), false)
})

test('改了主機、帳號、port、目錄、jobs、啟用：測試前要先存', () => {
  assert.equal(remoteCargoUnsaved(saved, form({ host: 'bogus-host.invalid' })), true)
  assert.equal(remoteCargoUnsaved(saved, form({ user: 'builder' })), true)
  assert.equal(remoteCargoUnsaved(saved, form({ port: '2222' })), true)
  assert.equal(remoteCargoUnsaved(saved, form({ root: '.cache/other' })), true)
  assert.equal(remoteCargoUnsaved(saved, form({ jobs: '8' })), true)
  assert.equal(remoteCargoUnsaved(saved, form({ enabled: false })), true)
})

test('新密碼、清除密碼也是沒存的改動', () => {
  assert.equal(remoteCargoUnsaved(saved, form({ password: 'secret' })), true)
  assert.equal(remoteCargoUnsaved(saved, form({ clearPassword: true })), true)
})

test('第一次設定：daemon 還沒存過任何東西，填好主機帳號按測試就要先存（不然回 400「host 尚未設定」）', () => {
  const empty: RemoteCargoSettings = { ...saved, enabled: false, host: '', user: '', password_set: false }
  assert.equal(remoteCargoUnsaved(empty, form({ enabled: false, host: '10.0.0.5', user: 'builder' })), true)
})

test('送出的 body：預設值、trim、密碼三態', () => {
  assert.deepEqual(toRemoteCargoInput(form({ host: ' h ', user: ' u ', port: '', root: '  ', jobs: '0' })), {
    enabled: true,
    host: 'h',
    user: 'u',
    ssh_port: 22,
    remote_root: '.cache/agents-manager/remote-cargo',
    cargo_jobs: 4,
  })
  assert.equal(toRemoteCargoInput(form({ password: 'p' })).password, 'p')
  assert.equal(toRemoteCargoInput(form({ clearPassword: true })).password, '')
  assert.equal('password' in toRemoteCargoInput(form()), false)
})
