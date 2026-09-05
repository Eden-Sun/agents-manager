// SPEC §13 project group chat — mock-mode walkthrough with headless Chrome (CDP).
// Prereq (in web/): VITE_MOCK=1 npx vite --port 5185
// Run: node scripts/demo-group.mjs   → docs/screenshots/140-*.png
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const URL_BASE = process.env.DEMO_URL ?? 'http://127.0.0.1:5185/'
const OUT = resolve(dirname(fileURLToPath(import.meta.url)), '../docs/screenshots')
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9377
const chrome = spawn(
  CHROME,
  [
    '--headless=new',
    `--remote-debugging-port=${PORT}`,
    '--disable-gpu',
    '--hide-scrollbars',
    '--no-first-run',
    '--user-data-dir=/tmp/am-cdp-group-5185',
    '--window-size=1280,860',
    URL_BASE,
  ],
  { stdio: 'ignore' },
)
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws
let id = 0
const pending = new Map()
const events = []
const send = (m, p = {}) => {
  const i = ++id
  ws.send(JSON.stringify({ id: i, method: m, params: p }))
  return new Promise((res, rej) => pending.set(i, { res, rej }))
}
// Refuse to drive somebody else's Chrome: the port must be ours (our process is alive).
chrome.on('exit', (code) => {
  console.error(`chrome exited early (code ${code}); is port ${PORT} already in use?`)
  process.exit(2)
})
for (let i = 0; i < 80; i++) {
  try {
    const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json()
    const p = l.find((t) => t.type === 'page' && t.url.startsWith('http'))
    if (p) {
      ws = new WebSocket(p.webSocketDebuggerUrl)
      break
    }
  } catch {}
  await sleep(250)
}
ws.onmessage = (e) => {
  const m = JSON.parse(e.data)
  if (m.id && pending.has(m.id)) {
    const p = pending.get(m.id)
    pending.delete(m.id)
    m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result)
  } else events.push(m)
}
await new Promise((r) => (ws.onopen = r))
await send('Runtime.enable')
await send('Page.enable')
const metrics = (w, h, dark = false) =>
  send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 2, mobile: false }).then(() =>
    send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: dark ? 'dark' : 'light' }] }),
  )
await metrics(1280, 860)
await send('Page.navigate', { url: URL_BASE })
await sleep(2500)
const ev = async (expr) => {
  const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true })
  return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value
}
const shot = async (name) => {
  await sleep(350)
  const { data } = await send('Page.captureScreenshot', { format: 'png' })
  writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64'))
  console.log('saved', name)
}
// React-controlled textarea: set the value through the prototype setter so onChange fires.
const type = (sel, value) =>
  ev(
    `(()=>{const el=document.querySelector(${JSON.stringify(sel)});if(!el)return 'no el';` +
      `Object.getOwnPropertyDescriptor(el.__proto__,'value').set.call(el,${JSON.stringify(value)});` +
      `el.setSelectionRange(el.value.length,el.value.length);el.dispatchEvent(new Event('input',{bubbles:true}));return 'typed'})()`,
  )
const key = (k, code) =>
  send('Input.dispatchKeyEvent', { type: 'keyDown', key: k, code: code ?? k, windowsVirtualKeyCode: k === 'Enter' ? 13 : k === 'ArrowDown' ? 40 : k === 'ArrowUp' ? 38 : 0 }).then(() =>
    send('Input.dispatchKeyEvent', { type: 'keyUp', key: k, code: code ?? k, windowsVirtualKeyCode: k === 'Enter' ? 13 : k === 'ArrowDown' ? 40 : k === 'ArrowUp' ? 38 : 0 }),
  )
const timeline = () =>
  ev(
    `[...document.querySelectorAll('.msg-list.group .msg')].map(a=>{const f=a.querySelector('.msg-from')?.textContent?.trim()??'';const b=a.querySelector('.bubble')?.textContent?.trim().slice(0,50)??'';return a.className.replace('msg ','')+' | '+f+' | '+b}).join('\\n')`,
  )

// 1. start both bots (the second one from the sidebar), then open the group view
console.log('bots:', await ev(`[...document.querySelectorAll('.bot-name')].map(e=>e.textContent).join(', ')`))
await ev(`[...document.querySelectorAll('.bot-row .mini-btn.primary')].forEach(b=>b.click())`)
await sleep(2200)
console.log('lamps:', await ev(`[...document.querySelectorAll('.bot-row .lamp')].map(e=>e.className).join(', ')`))
await ev(`document.querySelector('.project-label-btn').click()`)
await sleep(700)
console.log('group head:', await ev(`document.querySelector('.group-head .main-title')?.textContent`), '| members:', await ev(`document.querySelectorAll('.members .member').length`))
await shot('140-group-empty')

// 2. no mention → send disabled + hint
await type('.group-composer textarea', 'hello everyone')
await sleep(300)
console.log('send disabled w/o mention:', await ev(`document.querySelector('.group-composer .send-btn').disabled`), '| hint:', await ev(`document.querySelector('.group-hint')?.textContent`))
await shot('141-group-no-mention')

// 3. autocomplete: type "@" → popup; ArrowDown, Enter picks am-codex
await type('.group-composer textarea', '')
await type('.group-composer textarea', '@')
await sleep(300)
console.log('popup items:', await ev(`[...document.querySelectorAll('.mention-item')].map(e=>e.textContent.trim()).join(' / ')`))
await shot('142-group-mention-popup')
await ev(`document.querySelector('.group-composer textarea').focus()`)
await key('ArrowDown')
await key('ArrowDown')
await sleep(150)
console.log('active:', await ev(`document.querySelector('.mention-item.active')?.textContent.trim()`))
await key('Enter')
await sleep(300)
console.log('after pick:', JSON.stringify(await ev(`document.querySelector('.group-composer textarea').value`)))

// 4. @all → both bots working, folded user bubble "→ @am-claude, @am-codex"
await type('.group-composer textarea', '@all Reply with exactly GROUP-OK')
await sleep(200)
console.log('targets hint:', await ev(`document.querySelector('.group-targets')?.textContent`))
await ev(`document.querySelector('.group-composer .send-btn').click()`)
await sleep(700)
console.log('timeline (working):\n' + (await timeline()))
await shot('143-group-all-working')
await sleep(2200)
console.log('timeline (replied):\n' + (await timeline()))
await shot('144-group-all-replied')

// 5. @am-claude only
await type('.group-composer textarea', '@am-claude, only you: reply PONG')
await sleep(200)
await ev(`document.querySelector('.group-composer .send-btn').click()`)
await sleep(2600)
console.log('timeline (single):\n' + (await timeline()))
await shot('145-group-single-target')

// 6. stop am-codex → @all → skipped note in the timeline
await ev(`(()=>{const rows=[...document.querySelectorAll('.bot-row')];const r=rows.find(x=>x.querySelector('.bot-name').textContent==='am-codex');r.querySelector('.mini-btn.danger').click();return 'stopped am-codex'})()`)
await sleep(1200)
await type('.group-composer textarea', '@all one of you is offline')
await sleep(200)
console.log('skip hint:', await ev(`document.querySelector('.group-hint')?.textContent`))
await shot('146-group-skip-hint')
await ev(`document.querySelector('.group-composer .send-btn').click()`)
await sleep(2600)
console.log('timeline (skipped):\n' + (await timeline()))
console.log('notice:', await ev(`[...document.querySelectorAll('.notice')].map(n=>n.textContent).join(' || ')`))
await shot('147-group-skipped')

// 7. unread: open am-claude's own chat, send from there, reply lands while the group view is closed
await ev(`(()=>{const rows=[...document.querySelectorAll('.bot-row')];rows.find(x=>x.querySelector('.bot-name').textContent==='am-claude').click();return 'selected am-claude'})()`)
await sleep(500)
await type('.composer textarea', 'direct message to claude')
await sleep(150)
await ev(`document.querySelector('.composer .send-btn').click()`)
await sleep(2600)
console.log('unread badge:', await ev(`document.querySelector('.unread-badge')?.textContent ?? '(none)'`))
await shot('148-group-unread-badge')
await ev(`document.querySelector('.project-label-btn').click()`)
await sleep(700)
console.log('unread after open:', await ev(`document.querySelector('.unread-badge')?.textContent ?? '(none)'`))
console.log('timeline (with direct msg):\n' + (await timeline()))
await shot('149-group-after-direct')

// 8. dark + narrow
await metrics(1280, 860, true)
await sleep(400)
await shot('14a-group-dark')
await metrics(900, 860, false)
await sleep(400)
await shot('14b-group-narrow-900')

for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log('EXCEPTION', JSON.stringify(e.params).slice(0, 300))
ws.close()
chrome.removeAllListeners('exit')
chrome.kill()
process.exit(0)
