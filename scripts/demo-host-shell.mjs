// 主機 shell（HostShellPanel）against a LIVE daemon: 從「環境設定 → 主機」按「開 shell」，
// 在本機與遠端各下一次指令，深色 / 淺色 / 手機寬度都拍一張。
//
// Prereq（用一個**隔離的** daemon，不要動使用者正在跑的那一個）:
//   AM_DATA_DIR=<dir> agents-managerd serve --config <dir>/config.toml   # listen 127.0.0.1:7799
//   cd web && VITE_DAEMON=http://127.0.0.1:7799 npx vite --port 5312 --strictPort
// Usage: node scripts/demo-host-shell.mjs [http://127.0.0.1:5312/] [遠端主機名稱]
import { spawn } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5312/'
const REMOTE = process.argv[3] ?? 'm4ptest'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots/host-shell'
mkdirSync(OUT, { recursive: true })
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9361
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-host-shell', '--window-size=1440,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')

const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name, w = 1440, h = 900) => { await sleep(400); const { data } = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: w, height: h, scale: 2 } }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const viewport = (w, h, scheme) => Promise.all([
  send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 2, mobile: w < 900 }),
  send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: scheme }] }),
])
const clickText = (t, sel = 'button') => ev(`(()=>{const e=[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim().includes(${JSON.stringify(t)}));if(!e)return 'MISSING '+${JSON.stringify(t)};e.click();return 'clicked'})()`)
const waitFor = async (sel, tries = 80) => { for (let i = 0; i < tries; i++) { if (await ev(`Boolean(document.querySelector(${JSON.stringify(sel)}))`)) return true; await sleep(250) } return false }
/** 打一行指令並按 Enter（React 受控 input，要走 value setter + input 事件）。 */
const runCmd = async (cmd) => {
  await ev(`(()=>{const el=document.querySelector('.shell-cmd');if(!el)return 'MISSING';const d=Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value');d.set.call(el,${JSON.stringify(cmd)});el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));return 'sent'})()`)
  await sleep(2200)
}
/** 開「環境設定」popup 裡的主機清單。 */
const openHosts = async () => {
  await clickText('環境設定')
  await waitFor('.hosts-panel')
  await sleep(500)
}
/** 某一列（`.host-row`）的「開 shell」。`local` 是第一列。 */
const openShellFor = async (host) => {
  const r = await ev(`(()=>{
    const rows=[...document.querySelectorAll('.host-row')];
    const row=${JSON.stringify(host)}==='local'
      ? rows.find(r=>r.classList.contains('local'))
      : rows.find(r=>(r.querySelector('.host-name')?.textContent||'').trim().startsWith(${JSON.stringify(host)}));
    if(!row)return 'MISSING row '+${JSON.stringify(host)};
    const b=[...row.querySelectorAll('button')].find(b=>b.textContent.trim().includes('開 shell'));
    if(!b)return 'MISSING button';
    if(b.disabled)return 'DISABLED';
    b.click();return 'clicked';
  })()`)
  console.log(`  開 shell(${host}):`, r)
  await waitFor('.shell-pane')
  await sleep(1600)
}

await viewport(1440, 900, 'dark')
await send('Page.navigate', { url: URL_BASE })
await sleep(2600)

console.log('== 主機清單裡的「開 shell」（深色）==')
await openHosts()
await shot('430-hosts-open-shell-dark')

console.log('== 本機 shell + echo hi（深色）==')
await openShellFor('local')
console.log('  cwd:', await ev(`document.querySelector('.shell-cwd')?.textContent`))
await runCmd('echo hi')
await runCmd('pwd && git rev-parse --abbrev-ref HEAD')
console.log('  終端行數:', await ev(`(document.querySelector('.shell-term')?.textContent||'').split('\\n').length`))
await shot('431-local-shell-dark')

console.log('== 含捲動歷史（深色）==')
await runCmd('seq 1 120')
await ev(`(()=>{const s=document.querySelector('.shell-bar select');const d=Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value');d.set.call(s,'recent_unwrapped');s.dispatchEvent(new Event('change',{bubbles:true}));return s.value})()`)
await sleep(1800)
await shot('432-local-shell-scrollback-dark')
// 切回「畫面」，後面幾張才是常態視圖
await ev(`(()=>{const s=document.querySelector('.shell-bar select');const d=Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value');d.set.call(s,'visible');s.dispatchEvent(new Event('change',{bubbles:true}));return s.value})()`)
await sleep(1500)

console.log('== 遠端 shell（淺色）==')
await viewport(1440, 900, 'light')
await sleep(600)
await openHosts()
await openShellFor(REMOTE)
console.log('  cwd:', await ev(`document.querySelector('.shell-cwd')?.textContent`))
await runCmd('hostname && gh auth status')
console.log('  終端內容有 m4p:', await ev(`(document.querySelector('.shell-term')?.textContent||'').includes('m4p')`))
await shot('433-remote-shell-light')

console.log('== ctrl+c（淺色）==')
await runCmd('sleep 300')
await clickText('ctrl+c', '.key-btn')
await sleep(1800)
console.log('  有 ^C:', await ev(`(document.querySelector('.shell-term')?.textContent||'').includes('^C')`))
await shot('434-remote-shell-ctrlc-light')

console.log('== 手機寬度（深色）==')
await viewport(430, 900, 'dark')
await sleep(1500)
await shot('435-remote-shell-mobile-dark', 430, 900)

console.log('== 鍵盤可達：Tab 走一遍面板 ==')
await viewport(1440, 900, 'dark')
await sleep(800)
console.log('  焦點順序:', await ev(`(async()=>{
  const seen=[];
  document.querySelector('.menu-btn')?.focus();
  for(let i=0;i<12;i++){
    const a=document.activeElement;
    seen.push((a?.className||a?.tagName||'?').toString().split(' ')[0]+':'+(a?.textContent||a?.getAttribute('aria-label')||'').trim().slice(0,8));
    const f=[...document.querySelectorAll('button:not([disabled]),input,select,a[href]')].filter(e=>e.offsetParent!==null);
    const n=f[f.indexOf(a)+1]; if(!n)break; n.focus();
  }
  return seen.join(' → ');
})()`))
console.log('== 結束 shell 的二次確認（深色）==')
await clickText('結束 shell')
await waitFor('.confirm-backdrop')
await sleep(500)
await shot('436-end-shell-confirm-dark')
await clickText('取消')
await sleep(600)

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 300))
ws.close(); chrome.kill(); process.exit(0)
