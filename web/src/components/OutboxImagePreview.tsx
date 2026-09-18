import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { blobFor } from '../lib/outboxPreviewCache'
import { previewPlacement } from '../lib/outboxList'

/** 「bot 給你的檔案」裡的圖檔滑過去浮出的預覽（使用者 2026-09-18）；抓圖與快取見 `lib/outboxPreviewCache`。 */
/** 預覽框上限：大圖也看得清楚（直式截圖常很高），但不超出視窗。 */
function maxBox() {
  return { width: Math.min(520, window.innerWidth - 16), height: Math.min(640, window.innerHeight - 16) }
}

export function OutboxImagePreview({
  botId,
  name,
  modified,
  anchor,
}: {
  botId: string
  name: string
  modified: number
  anchor: DOMRect
}) {
  const [src, setSrc] = useState<{ key: string; url: string | null }>({ key: '', url: null })
  const key = `${botId}/${name}/${modified}`
  const boxRef = useRef<HTMLDivElement>(null)
  const [pos, setPos] = useState(() =>
    previewPlacement(anchor, maxBox(), { width: window.innerWidth, height: window.innerHeight }),
  )

  useEffect(() => {
    let alive = true
    blobFor(botId, name, modified).then(
      (url) => alive && setSrc({ key, url }),
      () => alive && setSrc({ key, url: null }),
    )
    return () => {
      alive = false
    }
  }, [botId, name, modified, key])

  // 圖載入後框的實際大小可能比上限小：用真的大小重算一次位置，框才會貼著那一列。
  useLayoutEffect(() => {
    const el = boxRef.current
    if (!el) return
    const r = el.getBoundingClientRect()
    setPos(
      previewPlacement(
        anchor,
        { width: r.width || maxBox().width, height: r.height || maxBox().height },
        { width: window.innerWidth, height: window.innerHeight },
      ),
    )
  }, [anchor, src])

  const ready = src.key === key
  const box = maxBox()
  return createPortal(
    <div
      ref={boxRef}
      className="outbox-preview"
      style={{ left: pos.left, top: pos.top, maxWidth: box.width, maxHeight: box.height }}
      role="tooltip"
    >
      {!ready ? (
        <span className="outbox-preview-note">載入預覽…</span>
      ) : src.url ? (
        <img src={src.url} alt={name} style={{ maxWidth: box.width - 12, maxHeight: box.height - 12 }} onLoad={() => setSrc((s) => ({ ...s }))} />
      ) : (
        <span className="outbox-preview-note">預覽載不出來</span>
      )}
    </div>,
    document.body,
  )
}
