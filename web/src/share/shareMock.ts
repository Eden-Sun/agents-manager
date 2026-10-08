/**
 * 分享頁的 mock（VITE_MOCK=1）：跟 `httpShareClient` 同一個介面，資料只在記憶體。
 * 開法：`VITE_MOCK=1 npx vite` 後開 `/share.html?token=demo_share_token_0123456789`；token 換成 `expired_…` 開頭演「連結已失效」。
 */
import type { ShareClient, ShareEvents } from './shareApi'
import { ShareHttpError, SHARE_FILE_MAX, SHARE_TEXT_MAX, type ShareFile, type ShareMessage, type ShareStatus } from './shareModel'

const ago = (min: number) => new Date(Date.now() - min * 60_000).toISOString()
let n = 0
const id = () => `msg_mock_${Date.now().toString(36)}_${(n++).toString(36)}`

/** 受限 bot 做的圖卡長這樣：只能寫 SVG，含中文與 emoji。 */
const MOCK_CARD = `<svg xmlns="http://www.w3.org/2000/svg" width="540" height="540" viewBox="0 0 540 540">
<rect width="540" height="540" rx="36" fill="#fff4d6"/>
<circle cx="430" cy="120" r="70" fill="#ffc94d"/>
<text x="60" y="260" font-size="64" font-weight="700" fill="#5b3a00">星期日早安 ☀️</text>
<text x="60" y="340" font-size="30" fill="#7a5a1c">慢慢來，今天也是好日子 🌿</text>
</svg>`

export function mockShareClient(token: string): ShareClient {
  const expired = token.startsWith('expired_')
  const messages: ShareMessage[] = [
    { id: 'm1', role: 'assistant', text: '你好，我是產品客服助理，可以回答安裝與帳號相關的問題，也可以看你上傳的設定檔。', created_at: ago(42), attachments: [] },
    { id: 'm2', role: 'user', text: '我的 config.toml 裝完之後一直說 port 被佔用，幫我看一下', created_at: ago(40), attachments: [{ name: 'config.toml' }] },
    {
      id: 'm3',
      role: 'assistant',
      text: '看了你的設定：`listen = "127.0.0.1:7788"` 跟另一個服務撞了。\n\n1. 改成 `7789`\n2. 重開服務\n\n改好的檔放在「bot 給你的檔案」裡，直接存下來覆蓋就好。',
      created_at: ago(39),
      attachments: [],
    },
    { id: 'm4', role: 'assistant', text: '另外幫你做了一張早安圖卡，就在下面。按「分享」可以直接傳給朋友。', created_at: ago(30), attachments: [] },
  ]
  const files: (ShareFile & { body: string })[] = [
    { name: 'config.toml', size: 418, modified_at: ago(39), version: 'mock-config-1', body: 'listen = "127.0.0.1:7789"\n' },
    { name: '安裝步驟.md', size: 2_310, modified_at: ago(120), version: 'mock-install-1', body: '# 安裝步驟\n' },
    { name: '星期日早安圖卡.svg', size: MOCK_CARD.length, modified_at: ago(30), version: 'mock-card-1', body: MOCK_CARD },
  ]
  const previews = new Map<string, string>()
  let status: ShareStatus = 'idle'
  let replying = false
  let subs: ShareEvents[] = []
  const guard = async () => {
    await new Promise((r) => setTimeout(r, 120))
    if (expired) throw new ShareHttpError(404)
  }
  const push = (m: ShareMessage) => {
    messages.push(m)
    for (const s of subs) s.onMessage(m)
  }
  const setStatus = (s: ShareStatus) => {
    status = s
    for (const x of subs) x.onStatus(s)
  }
  const uploads = new Map<string, string>()

  // 截圖用：`__shareMock.thinking()` 讓 bot 停在「思考中」。
  ;(globalThis as Record<string, unknown>).__shareMock = { thinking: () => setStatus('working'), idle: () => setStatus('idle') }

  return {
    async messages() {
      await guard()
      return { bot_name: 'support-bot', status, messages: [...messages], has_more: false }
    },
    async send(text, _crid, attachments) {
      await guard()
      if (text.length > SHARE_TEXT_MAX) throw new ShareHttpError(413)
      if (text.includes('429')) throw new ShareHttpError(429, 30)
      // 同 daemon：一段對話同時只排一則，上一則還沒回完就 409 not_accepted。
      if (status === 'working' || replying) throw new ShareHttpError(409)
      replying = true
      push({ id: id(), role: 'user', text, created_at: new Date().toISOString(), attachments: attachments.map((a) => ({ name: uploads.get(a) ?? a })) })
      setTimeout(() => setStatus('working'), 300)
      setTimeout(() => {
        push({ id: id(), role: 'assistant', text: `收到：「${text.slice(0, 40)}」。這是 mock 的回覆。`, created_at: new Date().toISOString(), attachments: [] })
        replying = false
        setStatus('idle')
      }, 2600)
    },
    async upload(file) {
      await guard()
      if (file.size > SHARE_FILE_MAX) throw new ShareHttpError(413)
      const att = `att_${uploads.size + 1}`
      uploads.set(att, file.name)
      return { id: att, name: file.name }
    },
    async files() {
      await guard()
      return files.map(({ body: _b, ...f }) => f)
    },
    fileUrl(name) {
      const f = files.find((x) => x.name === name)
      return URL.createObjectURL(new Blob([f?.body ?? ''], { type: 'application/octet-stream' }))
    },
    previewUrl(name) {
      const f = files.find((x) => x.name === name)
      let url = previews.get(name)
      if (!url) {
        url = URL.createObjectURL(new Blob([f?.body ?? ''], { type: /\.svg$/i.test(name) ? 'image/svg+xml' : 'application/octet-stream' }))
        previews.set(name, url)
      }
      return url
    },
    async fileBlob(name) {
      await guard()
      const f = files.find((x) => x.name === name)
      if (!f) throw new ShareHttpError(404)
      return new Blob([f.body])
    },
    subscribe(ev) {
      subs.push(ev)
      return () => {
        subs = subs.filter((s) => s !== ev)
      }
    },
  }
}
