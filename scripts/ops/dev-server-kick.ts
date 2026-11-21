#!/usr/bin/env bun

import { appendFileSync, existsSync, openSync, unlinkSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'

const HOME = process.env.HOME || homedir()
const SHARED_REPO = process.env.AGM_REPO || join(HOME, 'project', 'agents-manager')
const REPO = process.env.AGM_DEV_REPO || `${SHARED_REPO}-main`
const PORT = 5173
const LISTEN_TOOL = process.env.AGM_DEV_LISTEN_TOOL || (process.platform === 'linux' ? 'ss' : 'lsof')
const URL = `http://127.0.0.1:${PORT}/`
const DIR = dirname(import.meta.dir)
const LOG = join(DIR, 'dev-server.log')
const PENDING = join(DIR, 'dev-server.pending-install')
const VITE = join(REPO, 'web/node_modules/vite/bin/vite.js')

const ts = () => new Date().toLocaleString('sv-SE').replace('T', ' ')
const log = (line: string) => appendFileSync(LOG, `${line}\n`)
const AGM_BIN = process.env.AGM_BIN || join(DIR, 'bin', 'agm')

async function alert(reason: string, detail: string) {
  try {
    const p = Bun.spawn([AGM_BIN, '--compact', 'ops-alert', '--source', 'dev-server', '--reason', reason, '--detail', detail], { stdout: 'ignore', stderr: 'ignore' })
    if ((await p.exited) !== 0) log(`${ts()} 推 ops-alert（${reason}）失敗，只留在這份 log`)
  } catch (e) {
    log(`${ts()} 推 ops-alert（${reason}）失敗：${e}，只留在這份 log`)
  }
}

async function alive(): Promise<boolean> {
  try {
    const res = await fetch(URL, { signal: AbortSignal.timeout(3000) })
    return res.ok
  } catch {
    return false
  }
}

async function sh(cmd: string[]): Promise<string> {
  const p = Bun.spawn(cmd, { stdout: 'pipe', stderr: 'ignore' })
  const out = await new Response(p.stdout).text()
  await p.exited
  return out.trim()
}

type Listener = { pid: string; addrs: string[]; cmd: string; ppid: string; pcmd: string }

function parseLsof(out: string): Map<string, string[]> {
  const byPid = new Map<string, string[]>()
  let cur = ''
  for (const line of out.split('\n')) {
    if (line.startsWith('p')) cur = line.slice(1)
    else if (line.startsWith('n') && cur) byPid.set(cur, [...(byPid.get(cur) ?? []), line.slice(1)])
  }
  return byPid
}

function parseSs(out: string): Map<string, string[]> {
  const byPid = new Map<string, string[]>()
  for (const line of out.split('\n')) {
    const cols = line.trim().split(/\s+/)
    const addr = cols[3]
    if (!addr) continue
    for (const m of line.matchAll(/pid=(\d+)/g)) byPid.set(m[1]!, [...(byPid.get(m[1]!) ?? []), addr])
  }
  return byPid
}

async function listeners(): Promise<Listener[]> {
  const byPid =
    LISTEN_TOOL === 'ss'
      ? parseSs(await sh(['ss', '-Hltnp', `sport = :${PORT}`]))
      : parseLsof(await sh(['lsof', '-nP', `-iTCP:${PORT}`, '-sTCP:LISTEN', '-Fpn']))
  const res: Listener[] = []
  for (const [pid, addrs] of byPid) {
    const ps = await sh(['ps', '-o', 'ppid=,command=', '-p', pid])
    const ppid = ps.trim().split(/\s+/)[0] ?? ''
    const cmd = ps.trim().replace(/^\s*\d+\s*/, '')
    const pcmd = ppid && ppid !== '1' ? await sh(['ps', '-o', 'command=', '-p', ppid]) : ''
    res.push({ pid, addrs, cmd, ppid, pcmd })
  }
  return res
}

const reachable = (l: Listener) => l.addrs.some(a => a.startsWith('*:') || a.startsWith('0.0.0.0:') || a.startsWith('[::]:'))
const isVite = (l: Listener) => /vite/.test(l.cmd)
const orphan = (l: Listener) => l.ppid === '1' || /(^|\/)systemd(\s.*)?\s--user(\s|$)/.test(l.pcmd)

const pidOf = () => listeners().then(ls => ls[0]?.pid ?? '')

async function git(...args: string[]): Promise<string> {
  const p = Bun.spawn(['git', '-C', REPO, ...args], { stdout: 'pipe', stderr: 'pipe' })
  const out = await new Response(p.stdout).text()
  const err = await new Response(p.stderr).text()
  if ((await p.exited) !== 0) throw new Error(`git ${args[0]}: ${err.trim().slice(0, 200)}`)
  return out.trim()
}

async function syncMain(): Promise<boolean> {
  if (!existsSync(join(REPO, '.git'))) {
    log(`== ${ts()} ${REPO} 不是 git worktree，跳過同步`)
    await alert('dev_worktree_missing', `${REPO} 不是 git worktree；照 scripts/ops/README.md 建 detached worktree 並 bun install`)
    return false
  }
  try {
    const before = await git('rev-parse', 'HEAD')
    const lockBefore = await git('rev-parse', 'HEAD:web/bun.lock').catch(() => '')
    await git('fetch', '-q', 'origin', 'main')
    await git('reset', '-q', '--hard', 'origin/main')
    const after = await git('rev-parse', 'HEAD')
    const lockAfter = await git('rev-parse', 'HEAD:web/bun.lock').catch(() => '')
    if (before !== after) log(`== ${ts()} 5173 同步到 origin/main ${before.slice(0, 7)}..${after.slice(0, 7)}`)
    const pending = existsSync(PENDING)
    if (before === after && !pending) return false
    if (lockBefore === lockAfter && !pending) return false
    const p = Bun.spawn(['bun', 'install', '--frozen-lockfile'], { cwd: join(REPO, 'web'), stdout: 'ignore', stderr: 'ignore' })
    const code = await p.exited
    if (code !== 0) {
      writeFileSync(PENDING, `${lockAfter}\n`)
      log(`${ts()} web/bun.lock 的 bun install 結束碼 ${code}，不重啟還在跑的 vite，下一輪重試`)
      return false
    }
    if (pending) unlinkSync(PENDING)
    log(`${ts()} web/bun.lock 變了，bun install 結束碼 0，重啟 vite`)
    return true
  } catch (e) {
    log(`== ${ts()} 同步 origin/main 失敗，沿用現有樹：${e}`)
    return false
  }
}

async function nodeBin(): Promise<string | null> {
  const pinned = join(HOME, '.local/bin/node')
  if (existsSync(pinned)) return pinned
  const found = await sh(['which', 'node'])
  return found ? found.split('\n')[0]! : null
}

let holders = await listeners()
const good = holders.find(reachable)
const mustRestart = await syncMain()
if (good && (await alive()) && !mustRestart) process.exit(0)
if (good && mustRestart && isVite(good) && orphan(good)) {
  try { process.kill(Number(good.pid)) } catch {}
  for (let i = 0; i < 5 && (await listeners()).length > 0; i++) await Bun.sleep(1000)
  holders = await listeners()
}

for (const l of holders) {
  const where = l.addrs.join(',')
  if (!isVite(l)) {
    log(`== ${ts()} port ${PORT} 被非 vite 的 pid ${l.pid}（${where}）占用，不動它：${l.cmd.slice(0, 200)}`)
    process.exit(0)
  }
  if (!orphan(l)) {
    log(`== ${ts()} port ${PORT} 被 pid ${l.pid}（ppid ${l.ppid}，${where}）以 loopback-only 占用，需人工處理`)
    process.exit(0)
  }
  log(`== ${ts()} 收掉孤兒 loopback-only vite pid ${l.pid}（${where}），改起一顆綁 0.0.0.0 的`)
  try {
    process.kill(Number(l.pid))
  } catch (e) {
    log(`${ts()} kill pid ${l.pid} 失敗：${e}，放棄這輪`)
    process.exit(0)
  }
}
if (holders.length > 0) {
  for (let i = 0; i < 5 && (await listeners()).length > 0; i++) await Bun.sleep(1000)
  if ((await listeners()).length > 0) {
    log(`${ts()} port ${PORT} 5 秒內沒放開，下一輪再試`)
    process.exit(0)
  }
}

const node = await nodeBin()
if (!node) {
  log(`== ${ts()} 找不到 node，不用 bun 代跑（會在代理錯誤時 crash），放棄這輪`)
  await alert('dev_node_missing', '找不到 node，dev server 起不來（不用 bun 代跑）；裝 node 或放到 ~/.local/bin/node')
  process.exit(0)
}
if (!existsSync(VITE)) {
  log(`== ${ts()} 找不到 ${VITE}（web/ 還沒 install？），放棄這輪`)
  await alert('dev_vite_missing', `找不到 ${VITE}；在 ${REPO}/web 跑 bun install --frozen-lockfile`)
  process.exit(0)
}

const ver = await sh([node, '--version'])
log(`== ${ts()} dev server 沒回應，用 node ${ver} 拉起 vite（綁 0.0.0.0，供手機/LAN 存取）`)
const fd = openSync(LOG, 'a')
const child = Bun.spawn([node, VITE, '--host', '0.0.0.0', '--port', String(PORT), '--strictPort'], {
  cwd: join(REPO, 'web'),
  stdout: fd,
  stderr: fd,
  detached: true,
})
child.unref()

for (let i = 0; i < 15; i++) {
  await Bun.sleep(1000)
  if (await alive()) {
    log(`${ts()} dev server 已起來（node，pid ${await pidOf()}）`)
    process.exit(0)
  }
}
log(`${ts()} 15 秒內沒起來，下一輪再試（見上方 vite 輸出）`)
process.exit(0)
