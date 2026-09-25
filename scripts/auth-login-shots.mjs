// auth 失敗那則訊息的「立即登入」截圖：mock 模式的 vite（`cd web && VITE_MOCK=1 npx vite --port 5198`）＋headless Chrome。
// `OUT=docs/screenshots/auth-cli-login node scripts/auth-login-shots.mjs`（`MOBILE=1` 出手機那張）。暫存 profile 用完就刪。
import { spawn } from 'node:child_process'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
const OUT = process.env.OUT ?? 'docs/screenshots/auth-cli-login'
const BASE = process.env.BASE ?? 'http://127.0.0.1:5198/'
const BOT = process.env.BOT ?? 'am-claude'
mkdirSync(OUT, { recursive: true })
const PORT = 9393
const profile = mkdtempSync(join(tmpdir(), 'am-authshot-'))
const chrome = spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', `--user-data-dir=${profile}`, '--window-size=1280,860', BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws, id = 0; const pending = new Map()
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
try {
  for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find((t) => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
  ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } }
  await new Promise((r) => (ws.onopen = r))
  await send('Runtime.enable'); await send('Page.enable')
  const ev = async (expr) => (await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true })).result?.value
  const shot = async (name) => { const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('saved', name) }
  const mobile = process.env.MOBILE === '1'
  await send('Emulation.setDeviceMetricsOverride', mobile ? { width: 390, height: 844, deviceScaleFactor: 2, mobile: true } : { width: 1280, height: 860, deviceScaleFactor: 2, mobile: false })
  if (mobile) await send('Emulation.setEmulatedMedia', { features: [{ name: 'hover', value: 'none' }] })
  await send('Page.navigate', { url: BASE }); await sleep(3000)
  const picked = await ev(`(()=>{const el=[...document.querySelectorAll('button,[role=button],a,li,div')].find(e=>e.children.length<4&&e.textContent.trim()===${JSON.stringify(BOT)});if(!el)return false;el.click();return true})()`)
  console.log('picked', picked)
  await sleep(1200)
  if (await ev(`(()=>{const b=[...document.querySelectorAll('button')].find(b=>b.textContent.trim()==='啟動');if(b){b.click();return true}return false})()`)) await sleep(2500)
  // 把 bot 綁到沒登入的 cc1，再送一則會收在「失敗收尾（帳號或授權）」的訊息。
  console.log('loggedOut', await ev(`(()=>{__amMock.loggedOut(${JSON.stringify(BOT)}, 'cc1');return true})()`))
  await sleep(500)
  const text = 'authfail 幫我把庫存 query 改成分頁'
  await ev(`(()=>{const t=document.querySelector('textarea');const set=Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set;set.call(t,${JSON.stringify(text)});t.dispatchEvent(new Event('input',{bubbles:true}));return true})()`)
  await sleep(200)
  await ev(`(()=>{const b=[...document.querySelectorAll('button')].find(b=>/^送出|傳送|Send/.test(b.textContent.trim())||b.getAttribute('aria-label')==='送出');if(b){b.click();return true}const t=document.querySelector('textarea');t.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));return false})()`)
  await sleep(3500)
  console.log('actions', await ev(`document.querySelectorAll('.auth-login-action').length`))
  await ev(`(()=>{const a=document.querySelector('.auth-login-action');a&&a.scrollIntoView({block:'center'});return true})()`)
  await sleep(300)
  await shot(mobile ? 'phone-auth-fail' : 'desktop-auth-fail')
  // 按「立即登入」：畫面切到主機 shell，裡面是 `claude auth login`（原 bot 的 pane 沒被碰）。
  await ev(`(()=>{const b=[...document.querySelectorAll('.auth-login-action button')].find(b=>b.textContent.trim()==='立即登入');b.click();return true})()`)
  await sleep(1500)
  await shot(mobile ? 'phone-login-shell' : 'desktop-login-shell')
  // 回對話按「登入好了，重試」：mock 的登入馬上成功，所以會重送上一則（使用者訊息多一則）。
  await ev(`(()=>{const b=[...document.querySelectorAll('button,[role=tab]')].find(b=>b.textContent.trim()==='對話');b&&b.click();return true})()`)
  await sleep(800)
  const before = await ev(`document.querySelectorAll('.msg.user').length`)
  await ev(`(()=>{const b=[...document.querySelectorAll('.auth-login-action button')].find(b=>b.textContent.trim()==='登入好了，重試');b.click();return true})()`)
  await sleep(1500)
  console.log('user messages', before, '->', await ev(`document.querySelectorAll('.msg.user').length`), 'last:', await ev(`[...document.querySelectorAll('.msg.user')].pop()?.textContent`))
} finally {
  chrome.kill()
  await sleep(300)
  rmSync(profile, { recursive: true, force: true })
}
process.exit(0)
