#!/usr/bin/env bun
// 確保開發用 dev server（web/ 的 vite，port 5173，綁 0.0.0.0 供手機/LAN 存取）一直在跑。
// 由排程每 60 秒跑一次；有回應就什麼都不做。macOS 是 launchd `com.agm.dev-server`
//（`~/Library/LaunchAgents/com.agm.dev-server.plist`，`StartInterval 60` ＋ `RunAtLoad`），
// Linux 是 systemd user timer `com.agm.dev-server.timer`（issue #677）。
// 規則見 docs/SPEC.md §18.1，安裝方式見 scripts/ops/README.md。
//
// 這份是**來源檔**：改行為改這裡再 install 到
// `~/.config/agents-manager/supervisor/AGM/bin/dev-server-kick.ts`，不要只改安裝目錄那份
//（issue #418：這支在 2026-09-24 之前只存在於安裝目錄，沒有版控也沒有測試）。
//
// 為什麼「看門狗用 bun、vite 用 node」不矛盾：
//   看門狗只做 fetch / lsof（Linux 是 ss）/ spawn，不當 HTTP 代理，bun 的 socket 差異碰不到。
//   vite 不行：bun 1.3.14 交給 HTTP upgrade handler 的 socket 沒有 Node 的 destroySoon，
//   vite 代理在 upgrade 回應結束時會呼叫它（proxyRes.on('end') → socket.destroySoon()），
//   於是正式 daemon 一重啟、代理目標斷線，vite 整個 crash
//   （2026-09-12：TypeError: socket.destroySoon is not a function，Bun v1.3.14）。
//   所以 vite 一律用 node 拉起，找不到 node 寧可這輪不起，也不拿 bun 代跑。
//
// 健康的定義是「LAN 上的手機連得到」：綁 *:5173 才算數。只綁 127.0.0.1／[::1] 的實例，
// 是孤兒 vite（ppid=1，或 Linux 上被 systemd --user 收養，起它的人已經走了）就收掉換一顆；還有父程序就只記錄，那是別人正在用的。
//
// 5188 那類 VITE_MOCK=1 實例是各 bot 自己的測試環境，不歸這支管，絕不碰。
//
// 5173 吃的是一棵**只跟 origin/main 的乾淨 worktree**（REPO），不是大家共用的 ~/project/agents-manager：
//   位置由共用樹推導：`${AGM_REPO:-~/project/agents-manager}-main`（跟其他 kick 同一個 `AGM_REPO` 慣例，
//   #676：以前寫死 /Users/m4p/…，換到 Linux 主機就找不到），`AGM_DEV_REPO` 可以整個指定。
//   共用樹 HEAD 落後、又有 20+ 個別人未提交的 WIP，永遠追不上 origin/main，使用者在手機上
//   永遠看不到剛推的東西（2026-09-12 使用者裁示：5173＝已合併的事實）。
//   每輪先 `git fetch && git reset --hard origin/main`——那棵樹沒有任何人的 WIP，reset 是安全的。
//   bot 要驗自己未提交的改動，用自己的 port（5188/VITE_MOCK 慣例），不要靠 5173。
//   web/bun.lock 變了就 bun install 並重啟 vite；只有原始碼變的話 vite 自己 HMR，不重啟。

import { appendFileSync, existsSync, openSync, unlinkSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'

const HOME = process.env.HOME || homedir()
const SHARED_REPO = process.env.AGM_REPO || join(HOME, 'project', 'agents-manager')
const REPO = process.env.AGM_DEV_REPO || `${SHARED_REPO}-main` // git worktree，detached，只跟 origin/main
const PORT = 5173
// 誰在聽 port：macOS 用 lsof；Linux 用 iproute2 的 ss——Ubuntu server 不保證裝了 lsof（#676）。
// `AGM_DEV_LISTEN_TOOL` 只給測試在另一個平台上跑同一套情境。
const LISTEN_TOOL = process.env.AGM_DEV_LISTEN_TOOL || (process.platform === 'linux' ? 'ss' : 'lsof')
const URL = `http://127.0.0.1:${PORT}/`
const DIR = dirname(import.meta.dir) // supervisor/AGM
const LOG = join(DIR, 'dev-server.log')
const PENDING = join(DIR, 'dev-server.pending-install')
const VITE = join(REPO, 'web/node_modules/vite/bin/vite.js')

const ts = () => new Date().toLocaleString('sv-SE').replace('T', ' ')
const log = (line: string) => appendFileSync(LOG, `${line}\n`)
/** agm CLI：預設是這支腳本旁邊的 `bin/agm`（＝已安裝的 AGM 目錄），`AGM_BIN` 可覆寫（測試用）。 */
const AGM_BIN = process.env.AGM_BIN || join(DIR, 'bin', 'agm')

/** 安靜放棄這輪之前先喊人（issue #859）：一則 durable inbox 事件，同 source+reason 由 daemon 每小時只收一則，這裡不用節流。 */
async function alert(reason: string, detail: string) {
  try {
    const p = Bun.spawn([AGM_BIN, '--compact', 'ops-alert', '--source', 'dev-server', '--reason', reason, '--detail', detail], { stdout: 'ignore', stderr: 'ignore' })
    if ((await p.exited) !== 0) log(`${ts()} 推 ops-alert（${reason}）失敗，只留在這份 log`)
  } catch (e) {
    log(`${ts()} 推 ops-alert（${reason}）失敗：${e}，只留在這份 log`)
  }
}

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

type Listener = { pid: string; addrs: string[]; cmd: string; ppid: string; pcmd: string }

/** `lsof -Fpn`：`p<pid>` 一行、`n<位址>` 一行。 */
function parseLsof(out: string): Map<string, string[]> {
  const byPid = new Map<string, string[]>()
  let cur = ''
  for (const line of out.split('\n')) {
    if (line.startsWith('p')) cur = line.slice(1)
    else if (line.startsWith('n') && cur) byPid.set(cur, [...(byPid.get(cur) ?? []), line.slice(1)])
  }
  return byPid
}

/** `ss -Hltnp`：`LISTEN 0 511 0.0.0.0:5173 0.0.0.0:* users:(("node",pid=12,fd=21),…)`，第 4 欄是本地位址。
 *  沒有 `pid=` 的列（別的使用者的 socket，ss 不給看）略過——跟 lsof 對別人的行程一樣看不到。 */
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

/** 5173 上的 LISTEN 程序，依 pid 收攏（同一顆 vite 常同時有 IPv4／IPv6 兩筆）。 */
async function listeners(): Promise<Listener[]> {
  // lsof 一律用 -Fpn（機器可讀）。人類格式的最後一欄是 `(LISTEN)` 不是位址，
  // 照欄位切會把每顆都誤判成 loopback-only（2026-09-12 踩過，健康的那顆被當孤兒收掉）。
  const byPid =
    LISTEN_TOOL === 'ss'
      ? parseSs(await sh(['ss', '-Hltnp', `sport = :${PORT}`]))
      : parseLsof(await sh(['lsof', '-nP', `-iTCP:${PORT}`, '-sTCP:LISTEN', '-Fpn']))
  const res: Listener[] = []
  for (const [pid, addrs] of byPid) {
    const ps = await sh(['ps', '-o', 'ppid=,command=', '-p', pid])
    const ppid = ps.trim().split(/\s+/)[0] ?? ''
    const cmd = ps.trim().replace(/^\s*\d+\s*/, '')
    // 父程序的指令：Linux 上孤兒掛在 `systemd --user` 底下（見 orphan），要看得出來。
    const pcmd = ppid && ppid !== '1' ? await sh(['ps', '-o', 'command=', '-p', ppid]) : ''
    res.push({ pid, addrs, cmd, ppid, pcmd })
  }
  return res
}

/** 綁在萬用位址（`*:5173` / `0.0.0.0:5173`，ss 的 IPv6 萬用是 `[::]:5173`）才是 LAN 上的手機連得到的。 */
const reachable = (l: Listener) => l.addrs.some(a => a.startsWith('*:') || a.startsWith('0.0.0.0:') || a.startsWith('[::]:'))
const isVite = (l: Listener) => /vite/.test(l.cmd)
/** 孤兒＝起它的程序已經結束，沒有 bot 還在用它——可以收：ppid=1，或（Linux）被這個使用者的 `systemd --user` 收養
 *  （user session 的 subreaper，detached 的行程掛在它底下而不是 pid 1；#1176，跟 browser_gc_linux.py 的 _is_orphan_parent 同一件事）。 */
const orphan = (l: Listener) => l.ppid === '1' || /(^|\/)systemd(\s.*)?\s--user(\s|$)/.test(l.pcmd)

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
  await alert('dev_node_missing', '找不到 node，dev server 起不來（不用 bun 代跑）；裝 node 或放到 ~/.local/bin/node')
  process.exit(0)
}
if (!existsSync(VITE)) {
  log(`== ${ts()} 找不到 ${VITE}（web/ 還沒 install？），放棄這輪`)
  await alert('dev_vite_missing', `找不到 ${VITE}；在 ${REPO}/web 跑 bun install --frozen-lockfile`)
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
  //（systemd 看的是 cgroup，detached 不夠，靠 unit 的 KillMode=process。）
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
