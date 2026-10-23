/**
 * 環境設定子分頁（issue #1922，SPEC §4.3）：
 * 供「Jev 建議」與「Claude API」共用的二層分頁切換器。
 * 樣式採用沉底軌道與選中卡片（比照 .tabs／.tab，定義於 envSubTabs.css）。
 *
 * 無障礙需求（見 1922-design.md）：
 * - role="tablist" 與 aria-label
 * - 每顆按鈕 role="tab", aria-selected, aria-controls
 * - 鍵盤導覽：ArrowRight / ArrowDown 切換至下一項，ArrowLeft / ArrowUp 切換至上一項，Home / End 到首尾，自動選中並 focus
 * - 切換子分頁時將所在主 tabpanel 的捲軸位置重設為 0
 */
import { useRef, type KeyboardEvent, type ReactNode } from 'react'
import './envSubTabs.css'

export interface EnvSubTabItem<T extends string = string> {
  id: T
  label: string
  panelId: string
  content: ReactNode
}

export interface EnvSubTabsProps<T extends string = string> {
  /** tablist 的 aria-label，例如「Jev 建議分類」或「Claude API 分類」 */
  ariaLabel: string
  /** 目前選中的子分頁 id */
  value: T
  /** 切換子分頁回呼 */
  onChange: (id: T) => void
  /** 子分頁清單 */
  items: EnvSubTabItem<T>[]
}

export function EnvSubTabs<T extends string = string>({
  ariaLabel,
  value,
  onChange,
  items,
}: EnvSubTabsProps<T>) {
  const listRef = useRef<HTMLDivElement>(null)

  const selectTab = (id: T) => {
    onChange(id)
    // 依設計說明：切換子分頁時把該主分頁的捲軸設回 0
    const scroller = listRef.current?.closest<HTMLElement>('[role="tabpanel"]')
    if (scroller) {
      scroller.scrollTop = 0
    }
  }

  const handleKeyDown = (e: KeyboardEvent<HTMLButtonElement>, currentIndex: number) => {
    let nextIndex: number | null = null
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') {
      nextIndex = (currentIndex + 1) % items.length
    } else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') {
      nextIndex = (currentIndex - 1 + items.length) % items.length
    } else if (e.key === 'Home') {
      nextIndex = 0
    } else if (e.key === 'End') {
      nextIndex = items.length - 1
    }

    if (nextIndex !== null) {
      e.preventDefault()
      const nextItem = items[nextIndex]
      selectTab(nextItem.id)
      const buttons = listRef.current?.querySelectorAll<HTMLButtonElement>('[role="tab"]')
      buttons?.[nextIndex]?.focus()
    }
  }

  return (
    <>
      <div ref={listRef} className="env-subtabs subtabs" role="tablist" aria-label={ariaLabel}>
        {items.map((item, idx) => {
          const selected = item.id === value
          const tabId = `${item.panelId}-tab`
          return (
            <button
              key={item.id}
              type="button"
              role="tab"
              id={tabId}
              aria-controls={item.panelId}
              aria-selected={selected}
              tabIndex={0}
              onClick={() => selectTab(item.id)}
              onKeyDown={(e) => handleKeyDown(e, idx)}
            >
              {item.label}
            </button>
          )
        })}
      </div>
      {items.map((item) => {
        const selected = item.id === value
        const tabId = `${item.panelId}-tab`
        return (
          <div
            key={item.id}
            id={item.panelId}
            role="tabpanel"
            aria-labelledby={tabId}
            hidden={!selected}
          >
            {item.content}
          </div>
        )
      })}
    </>
  )
}
