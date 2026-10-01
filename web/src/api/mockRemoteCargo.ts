/**
 * mock 的 `/build/remote*`（外部 Cargo 編譯主機，API.md「GET/PUT /api/build/remote」「POST …/test」「…/install-toolchain」）。
 * 規則照 daemon：PUT 的 `password` 省略＝沿用、`""`＝清掉、其他＝新密碼，且不回傳；`enabled` 卻沒填 host／user → 400；
 * `test` 測的是**已儲存**的設定。主機名帶 `nocargo`／`noclippy`／`unreach` 會演對應的結果（截圖與測試用）。
 */
import { ApiError } from './types'

type Rec = Record<string, unknown>

interface Saved {
  enabled: boolean
  host: string
  user: string
  ssh_port: number
  remote_root: string
  cargo_jobs: number
  password: string | null
}

export class MockRemoteCargo {
  private saved: Saved = { enabled: false, host: '', user: '', ssh_port: 22, remote_root: '.cache/agents-manager/remote-cargo', cargo_jobs: 4, password: null }
  /** 裝過工具鏈的主機（`nocargo` 主機裝完就有）。 */
  private installed = new Set<string>()

  private view() {
    const { password, ...rest } = this.saved
    return { ...rest, test_threads: 8, timeout_secs: 0, shared_idle_hours: 24, max_shared_dirs: 12, max_concurrent: 0, password_set: password !== null }
  }

  handle(method: string, path: string, b: Rec): unknown {
    if (path === '/build/remote' && method === 'GET') return this.view()
    if (path === '/build/remote' && method === 'PUT') return this.put(b)
    if (path === '/build/remote/test' && method === 'POST') return this.test()
    if (path === '/build/remote/install-toolchain' && method === 'POST') return this.install()
    return undefined
  }

  private put(b: Rec) {
    const next: Saved = {
      enabled: b.enabled === true,
      host: String(b.host ?? '').trim(),
      user: String(b.user ?? '').trim(),
      ssh_port: Number(b.ssh_port ?? 22) || 22,
      remote_root: String(b.remote_root ?? '') || this.saved.remote_root,
      cargo_jobs: Number(b.cargo_jobs ?? 4) || 4,
      password: typeof b.password !== 'string' ? this.saved.password : b.password === '' ? null : b.password,
    }
    if (next.enabled && (!next.host || !next.user)) {
      throw new ApiError(400, { error: 'bad_request', message: 'host／user 不可為空' }, 'bad request')
    }
    this.saved = next
    return this.view()
  }

  private test() {
    const { host, user, ssh_port, password } = this.saved
    if (!host || !user) throw new ApiError(400, { error: 'bad_request', message: '還沒設定外部 Cargo 主機' }, 'bad request')
    if (/unreach/i.test(host)) throw new ApiError(502, { error: 'bad_gateway', message: `ssh: connect to host ${host} port ${ssh_port}: Operation timed out` }, 'bad gateway')
    const cargoMissing = /nocargo/i.test(host) && !this.installed.has(host)
    const clippyMissing = !cargoMissing && /noclippy/i.test(host) && !this.installed.has(host)
    return {
      ok: true,
      output: cargoMissing ? 'OS=Linux\nARCH=x86_64\nCARGO=' : `OS=Linux\nARCH=x86_64\nCARGO=/home/${user}/.cargo/bin/cargo\ncargo 1.90.0`,
      password_auth: password !== null,
      os: 'Linux',
      arch: 'x86_64',
      cargo_path: cargoMissing ? null : `/home/${user}/.cargo/bin/cargo`,
      cargo_version: cargoMissing ? '' : 'cargo 1.90.0',
      cargo_missing: cargoMissing,
      clippy_version: cargoMissing || clippyMissing ? null : 'clippy 0.1.90',
      clippy_missing: clippyMissing,
    }
  }

  private install() {
    const { host } = this.saved
    if (!host) throw new ApiError(400, { error: 'bad_request', message: '還沒設定外部 Cargo 主機' }, 'bad request')
    const already = !/nocargo|noclippy/i.test(host) || this.installed.has(host)
    this.installed.add(host)
    return { ok: true, already_installed: already, cargo_version: 'cargo 1.90.0', cc_missing: false, clippy_missing: false, output: 'info: installed' }
  }
}
