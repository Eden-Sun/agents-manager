#!/usr/bin/env bun
// 確保開發用 dev server（web/ 的 vite，port 5173，綁 0.0.0.0 供手機/LAN 存取）一直在跑。
// 由 launchd `com.agm.dev-server`（`~/Library/LaunchAgents/com.agm.dev-server.plist`，
// `StartInterval 60` ＋ `RunAtLoad`）每 60 秒跑一次；有回應就什麼都不做。
// 規則見 docs/SPEC.md §18.1，安裝方式見 scripts/ops/README.md。
//
// 這份是**來源檔**：改行為改這裡再 install 到
// `~/.config/agents-manager/supervisor/AGM/bin/dev-server-kick.ts`，不要只改安裝目錄那份
//（issue #418：這支在 2026-09-24 之前只存在於安裝目錄，沒有版控也沒有測試）。
//
// 為什麼「看門狗用 bun、vite 用 node」不矛盾：
//   看門狗只做 fetch / lsof / spawn，不當 HTTP 代理，bun 的 socket 差異碰不到。
//   vite 不行：bun 1.3.14 交給 HTTP upgrade handler 的 socket 沒有 Node 的 destroySoon，
//   vite 代理在 upgrade 回應結束時會呼叫它（proxyRes.on('end') → socket.destroySoon()），
//   於是正式 daemon 一重啟、代理目標斷線，vite 整個 crash
//   （2026-09-12：TypeError: socket.destroySoon is not a function，Bun v1.3.14）。
//   所以 vite 一律用 node 拉起，找不到 node 寧可這輪不起，也不拿 bun 代跑。
//
// 健康的定義是「LAN 上的手機連得到」：綁 *:5173 才算數。只綁 127.0.0.1／[::1] 的實例，
// 是孤兒 vite（ppid=1，起它的人已經走了）就收掉換一顆；還有父程序就只記錄，那是別人正在用的。
//
// 5188 那類 VITE_MOCK=1 實例是各 bot 自己的測試環境，不歸這支管，絕不碰。
//
// 5173 吃的是一棵**只跟 origin/main 的乾淨 worktree**（REPO），不是大家共用的 ~/project/agents-manager：
//   共用樹 HEAD 落後、又有 20+ 個別人未提交的 WIP，永遠追不上 origin/main，使用者在手機上
//   永遠看不到剛推的東西（2026-09-12 使用者裁示：5173＝已合併的事實）。
//   每輪先 `git fetch && git reset --hard origin/main`——那棵樹沒有任何人的 WIP，reset 是安全的。
//   bot 要驗自己未提交的改動，用自己的 port（5188/VITE_MOCK 慣例），不要靠 5173。
//   web/bun.lock 變了就 bun install 並重啟 vite；只有原始碼變的話 vite 自己 HMR，不重啟。

import { appendFileSync, existsSync, openSync } from 'node:fs'
import { dirname, join } from 'node:path'

const REPO = '/Users/m4p/project/agents-manager-main' // git worktree，detached，只跟 origin/main
const PORT = 5173
const URL = `http://127.0.0.1:${PORT}/`
const DIR = dirname(import.meta.dir) // supervisor/AGM
const LOG = join(DIR, 'dev-server.log')
const VITE = join(REPO, 'web/node_modules/vite/bin/vite.js')

const ts = () => new Date().toLocaleString('sv-SE').replace('T', ' ')
const log = (line: string) => appendFileSync(LOG, `${line}\n`)

/** 健康檢查走 loopback 就夠：本機看得到就代表有在聽。 */
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

type Listener = { pid: string; addrs: string[]; cmd: string; ppid: string }

/** 5173 上的 LISTEN 程序，依 pid 收攏（同一顆 vite 常同時有 IPv4／IPv6 兩筆）。 */
async function listeners(): Promise<Listener[]> {
  // -Fpn = 機器可讀：`p<pid>` 一行、`n<位址>` 一行。人類格式的最後一欄是 `(LISTEN)` 不是位址，
  // 照欄位切會把每顆都誤判成 loopback-only（2026-09-12 踩過，健康的那顆被當孤兒收掉）。
  const out = await sh(['lsof', '-nP', `-iTCP:${PORT}`, '-sTCP:LISTEN', '-Fpn'])
  const byPid = new Map<string, string[]>()
  let cur = ''
  for (const line of out.split('\n')) {
    if (line.startsWith('p')) cur = line.slice(1)
    else if (line.startsWith('n') && cur) byPid.set(cur, [...(byPid.get(cur) ?? []), line.slice(1)])
  }
  const res: Listener[] = []
  for (const [pid, addrs] of byPid) {
    const ps = await sh(['ps', '-o', 'ppid=,command=', '-p', pid])
    const ppid = ps.trim().split(/\s+/)[0] ?? ''
    const cmd = ps.trim().replace(/^\s*\d+\s*/, '')
    res.push({ pid, addrs, cmd, ppid })
  }
  return res
}

/** 綁在萬用位址（`*:5173` / `0.0.0.0:5173`）才是 LAN 上的手機連得到的。 */
const reachable = (l: Listener) => l.addrs.some(a => a.startsWith('*:') || a.startsWith('0.0.0.0:'))
const isVite = (l: Listener) => /vite/.test(l.cmd)
/** ppid=1：起它的程序已經結束，沒有 bot 還在用它——可以收。 */
const orphan = (l: Listener) => l.ppid === '1'

const pidOf = () => listeners().then(ls => ls[0]?.pid ?? '')

async function git(...args: string[]): Promise<string> {
  const p = Bun.spawn(['git', '-C', REPO, ...args], { stdout: 'pipe', stderr: 'pipe' })
  const out = await new Response(p.stdout).text()
  const err = await new Response(p.stderr).text()
  if ((await p.exited) !== 0) throw new Error(`git ${args[0]}: ${err.trim().slice(0, 200)}`)
  return out.trim()
}

/** 把乾淨 worktree 同步到 origin/main。回傳是否需要重啟 vite（依賴變了）。 */
async function syncMain(): Promise<boolean> {
  if (!existsSync(join(REPO, '.git'))) {
    log(`== ${ts()} ${REPO} 不是 git worktree，跳過同步`)
    return false
  }
  try {
    const before = await git('rev-parse', 'HEAD')
    const lockBefore = await git('rev-parse', 'HEAD:web/bun.lock').catch(() => '')
    await git('fetch', '-q', 'origin', 'main')
    await git('reset', '-q', '--hard', 'origin/main')
    const after = await git('rev-parse', 'HEAD')
    if (before === after) return false
    const lockAfter = await git('rev-parse', 'HEAD:web/bun.lock').catch(() => '')
    log(`== ${ts()} 5173 同步到 origin/main ${before.slice(0, 7)}..${after.slice(0, 7)}`)
    if (lockBefore === lockAfter) return false
    const p = Bun.spawn(['bun', 'install', '--frozen-lockfile'], { cwd: join(REPO, 'web'), stdout: 'ignore', stderr: 'ignore' })
    log(`${ts()} web/bun.lock 變了，bun install 結束碼 ${await p.exited}，重啟 vite`)
    return true
  } catch (e) {
    log(`== ${ts()} 同步 origin/main 失敗，沿用現有樹：${e}`)
    return false
  }
}

async function nodeBin(): Promise<string | null> {
  const pinned = '/Users/m4p/.local/bin/node'
  if (existsSync(pinned)) return pinned
  const found = await sh(['which', 'node'])
  return found ? found.split('\n')[0]! : null
}

// 1. 健康 = 「對外可達」，不只是「127.0.0.1 有回應」：綁在 loopback 的 vite 本機看得到、
//    使用者的手機看不到，那不算在跑。
let holders = await listeners()
const good = holders.find(reachable)
const mustRestart = await syncMain()
if (good && (await alive()) && !mustRestart) process.exit(0)
if (good && mustRestart && isVite(good) && orphan(good)) {
  try { process.kill(Number(good.pid)) } catch {}
  for (let i = 0; i < 5 && (await listeners()).length > 0; i++) await Bun.sleep(1000)
  holders = await listeners()
}

// 2. 只綁 loopback 的錯誤實例：是孤兒 vite 就收掉換一顆對外的；還有父程序代表某個 bot 正在用，不碰。
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
// 等 port 真的放開（最多 5 秒），沒放開就交給下一輪，不要硬搶。
if (holders.length > 0) {
  for (let i = 0; i < 5 && (await listeners()).length > 0; i++) await Bun.sleep(1000)
  if ((await listeners()).length > 0) {
    log(`${ts()} port ${PORT} 5 秒內沒放開，下一輪再試`)
    process.exit(0)
  }
}

// 3. 找不到 node 或 vite 就跳過這輪——不拿 bun 代跑，那等於把 crash 裝回去。
const node = await nodeBin()
if (!node) {
  log(`== ${ts()} 找不到 node，不用 bun 代跑（會在代理錯誤時 crash），放棄這輪`)
  process.exit(0)
}
if (!existsSync(VITE)) {
  log(`== ${ts()} 找不到 ${VITE}（web/ 還沒 install？），放棄這輪`)
  process.exit(0)
}

// 4. 真的沒人聽才拉起。綁 0.0.0.0：使用者從手機／LAN 上的裝置連它，只綁 loopback 會連不到。
const ver = await sh([node, '--version'])
log(`== ${ts()} dev server 沒回應，用 node ${ver} 拉起 vite（綁 0.0.0.0，供手機/LAN 存取）`)
// vite 的輸出接到 log 的 fd（Bun.spawn 的 stdout 只吃 fd / 'inherit' / 'ignore' / null，
// 不吃 FileSink），'a' 保證是追加，不會把既有 log 截掉。
const fd = openSync(LOG, 'a')
const child = Bun.spawn([node, VITE, '--host', '0.0.0.0', '--port', String(PORT), '--strictPort'], {
  cwd: join(REPO, 'web'),
  stdout: fd,
  stderr: fd,
  // launchd 會在這支腳本結束後收掉整個 job 的程序群；detached + unref 讓 vite 活下去。
  detached: true,
})
child.unref()

// 最多等 15 秒；起不來就寫 log 交給下一輪，不在腳本裡重試迴圈
//（web/ 編不過時才不會每 5 分鐘炸一次）。
for (let i = 0; i < 15; i++) {
  await Bun.sleep(1000)
  if (await alive()) {
    log(`${ts()} dev server 已起來（node，pid ${await pidOf()}）`)
    process.exit(0)
  }
}
log(`${ts()} 15 秒內沒起來，下一輪再試（見上方 vite 輸出）`)
process.exit(0)
