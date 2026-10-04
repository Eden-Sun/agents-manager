import { useEffect, useMemo, useState } from 'react'
import type { ShareClient } from './shareApi'
import { canShareFile, displayName, isSvgName, pngName, shareOrSave, svgExternalRefs, SvgTaintedError, svgToPng, svgWellFormed } from './shareImage'
import type { ShareFile } from './shareModel'

/** 檔案換了內容（同名覆寫）預覽要跟著換：時間或大小變了就換網址。 */
const fileVersion = (f: ShareFile) => `${f.modified_at ?? ''}-${f.size}`

/**
 * `external`＝圖引用外部資源（為了讀者的隱私不轉、不分享，永遠不會好）；`broken`＝這一版載不出來（bot 把圖寫壞了、網路斷了），
 * bot 修好、檔案換版就會重試——兩種講法不同（使用者 2026-10-04：長輩看到「沒辦法分享」以為圖不能分享）。
 */
type PngResult = { blob: Blob } | 'external' | 'broken'

/**
 * 向量圖 → 點陣圖每個檔（同版本）只轉一次，清單、對話、放大檢視共用。一出現就先轉好：`navigator.share` 要在點擊的
 * 同一個手勢裡呼叫，不能點了才開始轉。抓不到檔（網路）不留快取，下次再試。
 */
const pngCache = new WeakMap<ShareClient, Map<string, Promise<PngResult>>>()

function pngOf(client: ShareClient, name: string, version: string): Promise<PngResult> {
  let byKey = pngCache.get(client)
  if (!byKey) pngCache.set(client, (byKey = new Map()))
  const key = `${name}\n${version}`
  let p = byKey.get(key)
  if (!p) {
    p = client.fileBlob(name).then(
      async (b): Promise<PngResult> => {
        const text = await b.text()
        // 寫壞的先講「還在修」：bot 修好、換版就會重試（daemon 也會提醒它修，SPEC §20）。
        if (!svgWellFormed(text)) return 'broken'
        if (svgExternalRefs(text)) return 'external'
        try {
          return { blob: await svgToPng(text) }
        } catch (e) {
          return e instanceof SvgTaintedError ? 'external' : 'broken'
        }
      },
      (): PngResult => {
        byKey?.delete(key)
        return 'broken'
      },
    )
    byKey.set(key, p)
  }
  return p
}

function usePng(client: ShareClient, file: ShareFile, enabled: boolean): PngResult | 'loading' {
  // file 物件每次重抓清單都換新；名字與版本沒變就是同一張圖。結果帶著 key，換圖時舊結果自然不算數。
  const name = file.name
  const version = fileVersion(file)
  const key = `${name}\n${version}`
  const [res, setRes] = useState<{ key: string; r: PngResult } | null>(null)
  useEffect(() => {
    if (!enabled) return
    let alive = true
    void pngOf(client, name, version).then((r) => {
      if (alive) setRes({ key: `${name}\n${version}`, r })
    })
    return () => {
      alive = false
    }
  }, [client, name, version, enabled])
  return res?.key === key ? res.r : 'loading'
}

/** 點陣圖原樣交出去：先抓好，點擊時直接分享（同 [`usePng`] 的手勢理由）。 */
function useRaster(client: ShareClient, file: ShareFile, enabled: boolean): PngResult | 'loading' {
  const name = file.name
  const key = `${name}\n${fileVersion(file)}`
  const [res, setRes] = useState<{ key: string; r: PngResult } | null>(null)
  useEffect(() => {
    if (!enabled) return
    let alive = true
    client.fileBlob(name).then(
      (blob) => {
        if (alive) setRes({ key, r: { blob } })
      },
      () => {
        if (alive) setRes({ key, r: 'broken' })
      },
    )
    return () => {
      alive = false
    }
  }, [client, name, key, enabled])
  return res?.key === key ? res.r : 'loading'
}

/**
 * 圖片下面那顆大按鈕。分享頁的使用者是不懂電腦的長輩：畫面上不出現任何格式名稱或技術字（使用者 2026-10-04）。
 * 手機能分享檔案就叫「分享」（系統選單：傳 LINE、存相簿），不行就叫「存到手機」（直接存點陣圖）。
 * bot 畫的向量圖先在瀏覽器轉成點陣圖再交出去；原檔從不出現在畫面上。引用外部資源的只說「這張圖沒辦法分享」；
 * 這一版載不出來（bot 寫壞了，daemon 會提醒它修）說「這張圖還在修，請稍等」，不給按鈕，檔案換版就自動重試。
 */
export function ShareButton({ file, client }: { file: ShareFile; client: ShareClient }) {
  const vector = isSvgName(file.name)
  const png = usePng(client, file, vector)
  const raster = useRaster(client, file, !vector)
  const res = vector ? png : raster
  const [note, setNote] = useState<string | null>(null)
  const outName = vector ? pngName(file.name) : file.name
  const ready = res !== 'loading' && res !== 'external' && res !== 'broken'
  const type = ready ? (vector ? 'image/png' : res.blob.type || 'image/png') : 'image/png'
  const share = useMemo(() => (ready ? canShareFile(new File([res.blob], outName, { type })) : false), [ready, res, outName, type])
  if (res === 'external') return <span className="sh-png-note">這張圖沒辦法分享</span>
  if (res === 'broken') return <span className="sh-png-note sh-png-fixing">這張圖還在修，請稍等</span>
  return (
    <span className="sh-png">
      <button
        type="button"
        className="sh-png-btn"
        disabled={!ready}
        aria-busy={!ready}
        onClick={() => {
          if (!ready) return
          void shareOrSave(res.blob, outName, type).then((r) => setNote(r === 'saved' ? '存好了' : null))
        }}
      >
        {share ? '分享' : '存到手機'}
      </button>
      {note ? (
        <span className="sh-png-note" role="status">
          {note}
        </span>
      ) : null}
    </span>
  )
}

/** 縮圖：一律 `<img>`（向量圖當圖片載入不跑 script），點了放大。載不出來顯示佔位，不擋其他東西；檔案換版（bot 修好）就重載。 */
export function ShareThumb({ file, client, onOpen, big }: { file: ShareFile; client: ShareClient; onOpen: (f: ShareFile) => void; big?: boolean }) {
  const version = fileVersion(file)
  const [brokenAt, setBrokenAt] = useState<string | null>(null)
  const broken = brokenAt === version
  return (
    <button type="button" className={`sh-thumb${big ? ' big' : ''}`} aria-label={`放大 ${displayName(file.name)}`} title={displayName(file.name)} onClick={() => onOpen(file)}>
      {broken ? (
        <span className="sh-thumb-broken" title="這張圖還在修，請稍等">
          🖼
        </span>
      ) : (
        <img key={version} src={client.previewUrl(file.name, version)} alt={big ? displayName(file.name) : ''} loading="lazy" decoding="async" onError={() => setBrokenAt(version)} />
      )}
    </button>
  )
}

/** 放大檢視：大圖＋同一顆「分享／存到手機」。 */
export function ShareImageViewer({ file, client, onClose }: { file: ShareFile; client: ShareClient; onClose: () => void }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  return (
    <div
      className="sh-viewer"
      role="dialog"
      aria-modal="true"
      aria-label={displayName(file.name)}
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose()
      }}
    >
      <div className="sh-viewer-bar">
        <span className="sh-viewer-name">{displayName(file.name)}</span>
        <button type="button" className="sh-viewer-close" aria-label="關閉" onClick={onClose}>
          ✕
        </button>
      </div>
      <img className="sh-viewer-img" src={client.previewUrl(file.name, fileVersion(file))} alt={displayName(file.name)} />
      <div className="sh-viewer-actions">
        <ShareButton file={file} client={client} />
      </div>
    </div>
  )
}
