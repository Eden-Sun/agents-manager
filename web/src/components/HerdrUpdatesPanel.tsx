import { useState } from 'react'
import type { HerdrHost, HerdrUpdates, Standing, VersionSide } from '../api/herdrUpdates'
import { useHerdrUpdates } from '../hooks/useHerdrUpdates'
import './herdrUpdates.css'

/**
 * 環境設定裡的「Herdr 版本」卡（2026-09-16 使用者：Herdr 版本也要追新功能與通知）。
 *
 * 只讀：按鈕最多「重新檢查」與官方 release 連結。真正的升級要停 server、會殺掉正在跑的 pane，
 * 那是 AGM 走既有運維流程的事，這張卡不做（`docs/UI-DECISIONS.md`）。
 *
 * 三個版本分開顯示（跑著的 server / 磁碟上的 CLI / 官方 stable），而且**未知就寫未知**：這張卡存在的
 * 理由就是不要讓人以為自己是最新的。
 */

const STANDING_LABEL: Record<Standing, string> = {
  latest: '已是最新',
  behind: '有新版',
  ahead: '比官方新',
  unknown: '未知',
}

/** 主機清單與摘要句用同一個稱呼：`local` 在畫面上一律是「本機」。 */
function hostLabel(name: string): string {
  return name === 'local' ? '本機' : name
}

function ago(iso: string | null): string {
  if (!iso) return '從未'
  const ms = Date.now() - new Date(iso).getTime()
  if (!Number.isFinite(ms)) return '未知'
  if (ms < 60_000) return '剛剛'
  const mins = Math.floor(ms / 60_000)
  if (mins < 60) return `${mins} 分鐘前`
  const hours = Math.floor(mins / 60)
  if (hours < 48) return `${hours} 小時前`
  return `${Math.floor(hours / 24)} 天前`
}

/**
 * 一個版本（跑著的或磁碟上的）。讀不到就畫「未知」加原因，不留空白讓人自己猜。
 *
 * 讀數不新鮮（離線、探測失敗、太久沒巡）時只寫「上次讀到 X」，狀態標籤是「未知」——**不能**拿舊值
 * 說它現在是最新的。歷史比較結果（`cachedStanding`）降級成灰字附註，不給綠燈。
 */
function SideRow({ label, side, title }: { label: string; side: VersionSide; title: string }) {
  const known = side.version !== null
  return (
    <div className="hu-side">
      <span className="hu-side-k" title={title}>
        {label}
      </span>
      <span className={`hu-ver${known && side.fresh ? '' : ' hu-unknown'}`}>
        {known ? (side.fresh ? side.version : `上次讀到 ${side.version}`) : '未知'}
      </span>
      <span className={`hu-tag hu-${side.standing}`}>{STANDING_LABEL[side.standing]}</span>
      {known && !side.fresh && side.cachedStanding !== 'unknown' ? (
        <span className="hu-when">（當時{STANDING_LABEL[side.cachedStanding]}）</span>
      ) : null}
      {known ? <span className="hu-when">{ago(side.at)}</span> : side.error ? null : <span className="hu-when">尚未查過</span>}
      {side.error ? <span className="hu-err">{side.error}</span> : null}
    </div>
  )
}

function HostCard({ host }: { host: HerdrHost }) {
  return (
    <li className="hu-host">
      <div className="hu-host-head">
        <span className="hu-host-name">{hostLabel(host.host)}</span>
        {host.connected ? null : <span className="hu-tag hu-unknown">未連線</span>}
        {host.connected && !host.fresh ? <span className="hu-tag hu-unknown">資料過期</span> : null}
        {host.restartPending ? (
          <span className="hu-tag hu-pending" title="磁碟上已是新版，換掉跑著的 server 才會生效">
            待套用
          </span>
        ) : null}
      </div>
      <SideRow label="跑著的" side={host.server} title="這台主機上正在跑的 herdr server（決定 pane 行為的就是它）" />
      <SideRow label="磁碟上" side={host.disk} title="這台主機磁碟上的 herdr CLI（herdr --version）" />
    </li>
  )
}

/** 版本差距的官方 release notes。缺資料要明說，不能把最新那段講成整段跨版的內容。 */
function Notes({ data }: { data: HerdrUpdates }) {
  const [open, setOpen] = useState(false)
  const { notes } = data
  if (notes.sections.length === 0 && !notes.gap) return null
  return (
    <div className="hu-notes">
      <button type="button" className="mini-btn" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        {open ? '收起更新內容' : `看更新內容${notes.sections.length > 1 ? `（${notes.sections.length} 版）` : ''}`}
      </button>
      {open ? (
        <div className="hu-notes-body">
          {notes.gap ? <p className="hu-warn">{notes.gap}</p> : null}
          {notes.missingNotes.length > 0 ? (
            <p className="hu-warn">官方沒有附這幾版的說明：{notes.missingNotes.join('、')}</p>
          ) : null}
          {notes.sections.map((s) => (
            <section key={s.version} className="hu-note">
              <h5>{s.version}</h5>
              {s.notes ? <pre>{s.notes}</pre> : <p className="hu-warn">官方沒有附這一版的說明。</p>}
            </section>
          ))}
          <p className="hu-src">
            來源：
            <a href={data.latest.sourceUrl} target="_blank" rel="noreferrer noopener">
              {data.latest.sourceUrl}
            </a>
          </p>
        </div>
      ) : null}
    </div>
  )
}

/**
 * 一句話的結論。順序就是「壞消息先講」，而且**只有在真的每一台都比對過**才說得出「都跟官方同版」：
 * 沒有主機、有人未知、有人資料過期、有人比官方新——每一種都得有自己的句子，不能落回那句綠燈。
 */
function summary(data: HerdrUpdates): string {
  const { latest, hosts, behindHosts, unknownHosts, staleHosts } = data
  if (latest.version === null) return '還不知道官方最新版是哪一版，所以也不能說主機是最新的。'
  if (latest.stale) return `官方版本是上次查到的（${ago(latest.fetchedAt)}），還沒重新確認過，先不判斷各主機。`
  if (hosts.length === 0) return '還沒問到任何主機的 herdr 版本。'
  if (behindHosts.length > 0) {
    const rest = staleHosts.length > 0 ? `；另有 ${staleHosts.map(hostLabel).join('、')} 的資料已過期` : ''
    return `${behindHosts.length} 台主機落後：${behindHosts.map(hostLabel).join('、')}${rest}。`
  }
  if (unknownHosts.length > 0) {
    const stale = unknownHosts.filter((h) => staleHosts.includes(h))
    const why = stale.length === unknownHosts.length ? '的資料已過期' : '的版本問不到'
    return `${unknownHosts.map(hostLabel).join('、')}${why}，先當未知——還不能說全部都是最新的。`
  }
  if (hosts.every((h) => h.server.standing === 'ahead' || h.disk.standing === 'ahead')) {
    return '主機上的版本比官方 stable 新（prerelease 或自行建置）。'
  }
  if (hosts.some((h) => h.server.standing === 'ahead' || h.disk.standing === 'ahead')) {
    return '沒有主機落後；部分主機比官方 stable 新（prerelease 或自行建置）。'
  }
  return '所有主機都跟官方同版。'
}

export function HerdrUpdatesPanel() {
  const { data, loading, refreshing, refresh, markSeen } = useHerdrUpdates()
  if (loading) return <p className="hu-muted">讀取中…</p>
  if (!data) return null
  const { latest } = data
  const behind = data.behindHosts.length
  return (
    <div className="hu">
      <div className="hu-top">
        <div className="hu-latest">
          <span className="hu-side-k">官方最新</span>
          <span className={`hu-ver ${latest.version ? '' : 'hu-unknown'}`}>{latest.version ?? '未知'}</span>
          {latest.stale && latest.version ? (
            <span className="hu-tag hu-unknown" title={`上次查到是 ${ago(latest.fetchedAt)}`}>
              舊資料
            </span>
          ) : null}
          <span className="hu-when">查於 {ago(latest.fetchedAt)}</span>
        </div>
        <div className="hu-actions">
          <button type="button" className="mini-btn" disabled={refreshing} onClick={refresh}>
            {refreshing ? '檢查中…' : '重新檢查'}
          </button>
          <a className="mini-btn" href={latest.releasesUrl} target="_blank" rel="noreferrer noopener">
            官方 release
          </a>
        </div>
      </div>

      {latest.error && !data.error ? <p className="hu-err-row">查不到官方版本：{latest.error}</p> : null}
      {data.error ? (
        <p className="hu-err-row">
          {data.unsupported ? '這個 daemon 沒有 Herdr 版本追蹤' : '暫時連不上 daemon，以下是上次拿到的資料'}：{data.error}
        </p>
      ) : null}

      <p className={behind > 0 ? 'hu-head-line hu-behind-line' : 'hu-head-line'}>{summary(data)}</p>

      <ul className="hu-hosts">
        {data.hosts.length === 0 ? <li className="hu-muted">還沒問到任何主機的 herdr 版本。</li> : null}
        {data.hosts.map((h) => (
          <HostCard key={h.host} host={h} />
        ))}
      </ul>

      <Notes data={data} />

      <p className="hu-foot">
        只追蹤與通知，不自動升級。0.9 起 client 更新可以不動相容的 server 與它底下的 pane；endpoint
        generation 較舊的 server 需要一次性升級，那次才會影響正在跑的 pane。由 AGM 確認版本相容性後安排時間。
        {data.notice?.notifiedAt ? ` 已於 ${ago(data.notice.notifiedAt)}通知 AGM。` : ''}
      </p>
      {data.unread ? (
        <button type="button" className="mini-btn hu-seen" onClick={markSeen}>
          知道了，不用再提示 {latest.version}
        </button>
      ) : null}
    </div>
  )
}

/**
 * 側欄「環境設定」那一行右邊的未讀點。不進主 header：那裡已經夠擠，而且這件事不急到要一直看著。
 * 跟面板共用同一份資料（`useHerdrUpdates`），所以按掉之後兩邊一起消失。
 */
export function HerdrUpdateDot() {
  const { data } = useHerdrUpdates()
  if (!data?.unread || !data.latest.version) return null
  return (
    <span className="hu-dot" title={`Herdr ${data.latest.version} 可用（${data.behindHosts.map(hostLabel).join('、')} 落後）`}>
      Herdr {data.latest.version}
    </span>
  )
}
