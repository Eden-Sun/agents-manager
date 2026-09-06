// Walk every page in mock mode and flag layout problems mechanically:
// overlapping siblings, content escaping its container, and anything past the viewport.
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5411 --strictPort`
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.DEMO_URL ?? 'http://127.0.0.1:5411/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9381
const WIDTH = Number(process.env.W ?? 1600)
const chrome = spawn(CHROME, ['--headless=new',`--remote-debugging-port=${PORT}`,'--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-audit',`--window-size=${WIDTH},1000`, URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:WIDTH,height:1000,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2600)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? 'eval failed'); return r.result?.value }
const shot = async (name, h = 1000) => { await sleep(320); const { data } = await send('Page.captureScreenshot',{format:'png',clip:{x:0,y:0,width:WIDTH,height:h,scale:2}}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('  saved', name) }
const waitFor = async (sel) => { for (let i=0;i<80;i++){ if (await ev(`Boolean(document.querySelector(${JSON.stringify(sel)}))`)) return true; await sleep(120) } return false }

/**
 * Three mechanical checks, run over every visible element:
 *  - siblings whose boxes overlap (the team member strip failed exactly this way),
 *  - a child painting outside a parent that is not set up to clip or scroll,
 *  - anything sticking out past the right edge of the window.
 */
const AUDIT = `(() => {
  const vis = (el) => {
    const r = el.getBoundingClientRect()
    if (r.width < 1 || r.height < 1) return false
    const cs = getComputedStyle(el)
    return cs.visibility !== 'hidden' && cs.display !== 'none' && Number(cs.opacity) > 0.05
  }
  const name = (el) => el.tagName.toLowerCase() + (el.className && typeof el.className === 'string' ? '.' + el.className.trim().split(/\\s+/).slice(0, 3).join('.') : '')
  const out = { overlap: [], escape: [], offscreen: [] }
  // Paths inside an icon overlap by design; only element-level layout is interesting here.
  const all = [...document.querySelectorAll('body *')].filter((e) => vis(e) && !e.closest('svg'))

  // overlapping siblings, inline-level boxes only (grid/absolute layers legitimately stack)
  for (const parent of new Set(all.map((e) => e.parentElement).filter(Boolean))) {
    const cs = getComputedStyle(parent)
    if (cs.display !== 'flex' && cs.display !== 'block' && cs.display !== 'inline-flex') continue
    const kids = [...parent.children].filter((k) => vis(k) && getComputedStyle(k).position === 'static')
    for (let i = 0; i < kids.length; i += 1) {
      for (let j = i + 1; j < kids.length; j += 1) {
        const a = kids[i].getBoundingClientRect(), b = kids[j].getBoundingClientRect()
        const ox = Math.min(a.right, b.right) - Math.max(a.left, b.left)
        const oy = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top)
        if (ox > 2 && oy > 2) out.overlap.push(\`\${name(kids[i])} × \${name(kids[j])} (\${Math.round(ox)}×\${Math.round(oy)}px) in \${name(parent)}\`)
      }
    }
  }

  // children painting outside a parent that neither clips nor scrolls
  for (const el of all) {
    const p = el.parentElement
    if (!p || p === document.body) continue
    const cs = getComputedStyle(p)
    if (cs.overflow !== 'visible' || cs.position === 'absolute' || getComputedStyle(el).position !== 'static') continue
    const a = el.getBoundingClientRect(), b = p.getBoundingClientRect()
    if (b.width < 2) continue
    const over = Math.round(Math.max(a.right - b.right, b.left - a.left))
    if (over > 6) out.escape.push(\`\${name(el)} sticks \${over}px out of \${name(p)}\`)
  }

  // past the right edge of the window
  for (const el of all) {
    const r = el.getBoundingClientRect()
    if (r.right - window.innerWidth > 2 && r.width < window.innerWidth) out.offscreen.push(\`\${name(el)} right=\${Math.round(r.right)} > \${window.innerWidth}\`)
  }
  const uniq = (xs) => [...new Set(xs)].slice(0, 6)
  return JSON.stringify({ overlap: uniq(out.overlap), escape: uniq(out.escape), offscreen: uniq(out.offscreen), hscroll: document.documentElement.scrollWidth > window.innerWidth })
})()`

const audit = async (label) => {
  const r = JSON.parse(await ev(AUDIT))
  const bad = r.overlap.length + r.escape.length + r.offscreen.length + (r.hscroll ? 1 : 0)
  console.log(`\n=== ${label} === ${bad === 0 ? 'clean' : `${bad} finding(s)`}`)
  for (const k of ['overlap', 'escape', 'offscreen']) for (const line of r[k]) console.log(`  [${k}] ${line}`)
  if (r.hscroll) console.log('  [hscroll] the document scrolls sideways')
  return r
}

// ---------------------------------------------------------------- 1. bot chat
await ev("document.querySelector('.bot-row')?.click()"); await sleep(700)
await ev("[...document.querySelectorAll('button')].find((b) => b.textContent.trim() === '啟動')?.click()")
await waitFor('.main-head .main-status'); await sleep(600)
await audit('bot chat (running)')
await shot('380-audit-bot-chat', 320)

// settings panel
await ev("document.querySelector('.main-head .icon-btn.gear')?.click()"); await sleep(600)
await audit('bot settings panel')
await ev("document.querySelector('.bs-close, .bot-settings .icon-btn')?.click()"); await sleep(400)

// terminal tab
await ev("[...document.querySelectorAll('.tab')].find((t) => t.textContent.includes('終端'))?.click()"); await sleep(700)
await audit('terminal tab')
await ev("[...document.querySelectorAll('.tab')].find((t) => t.textContent.includes('對話'))?.click()"); await sleep(500)

// ---------------------------------------------------------------- 2. group chat
await ev("document.querySelector('.project-head')?.click()"); await sleep(900)
await audit('group chat')
await shot('381-audit-group', 320)

// ---------------------------------------------------------------- 3. team launch
await ev("document.querySelector('.issues-btn')?.click()")
await waitFor('.issue-row .team-btn')
await ev("document.querySelector('.issue-row .team-btn')?.click()")
await waitFor('.team-launch'); await sleep(600)
await audit('team launch (組隊)')

// ---------------------------------------------------------------- 4. team panel
await ev("[...document.querySelectorAll('button')].find((b) => b.textContent.includes('建立並啟動'))?.click()")
await waitFor('.team-head .team-phase'); await sleep(1200)
await audit('team panel')
await shot('382-audit-team', 320)
console.log('  members:', await ev(`JSON.stringify([...document.querySelectorAll('.team-members .member')].map((m) => ({ text: m.textContent.trim(), w: Math.round(m.getBoundingClientRect().width), left: Math.round(m.getBoundingClientRect().left) })))`))

// ---------------------------------------------------------------- 5. sidebar popups
await ev("[...document.querySelectorAll('.sidebar-foot .btn')].find((b) => b.textContent.includes('新增 Project'))?.click()")
await waitFor('.modal'); await sleep(500)
await audit('新增 Project popup')
await ev("document.querySelector('.modal-head .icon-btn')?.click()"); await sleep(300)

await ev("[...document.querySelectorAll('.sidebar-foot .btn')].find((b) => b.textContent.includes('新增 Bot'))?.click()")
await waitFor('.modal'); await sleep(500)
await audit('新增 Bot popup')
await ev("document.querySelector('.modal-head .icon-btn')?.click()"); await sleep(300)

await ev("document.querySelector('.sidebar-foot .disclosure')?.click()")
await waitFor('.modal'); await sleep(600)
await audit('環境設定 popup')
await ev("document.querySelector('.modal-head .icon-btn')?.click()"); await sleep(300)

console.log('\n--- runtime errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,300))
ws.close(); chrome.kill(); process.exit(0)
