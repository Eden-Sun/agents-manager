import { useId, useState } from 'react'
import type { ReactNode } from 'react'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import './envPanelPhone.css'

/**
 * 環境設定裡的一區（issue #1466）。桌機：標題加內容，一直展開（跟以前一樣）。
 * 手機（≤640px）：標題變整列可點的鍵、預設收著，頁面收成一排標題，要哪一區點哪一區。
 * 收起用 `hidden` 不卸載——打了一半的表單、讀回來的設定都留著，樹的形狀也不隨視窗寬度改變。
 */
export function EnvSection({ title, children }: { title: string; children: ReactNode }) {
  const phone = useMediaQuery(PHONE_QUERY)
  const [open, setOpen] = useState(false)
  const bodyId = useId()
  return (
    <section className="env-sec">
      <h3>
        {phone ? (
          <button type="button" className="env-sec-toggle" aria-expanded={open} aria-controls={bodyId} onClick={() => setOpen(!open)}>
            <span>{title}</span>
            <span className="chev" aria-hidden="true">
              {open ? '▼' : '▶'}
            </span>
          </button>
        ) : (
          title
        )}
      </h3>
      <div className="env-sec-body" id={bodyId} hidden={phone && !open}>
        {children}
      </div>
    </section>
  )
}
