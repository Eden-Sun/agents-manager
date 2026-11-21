import { useRef, type KeyboardEvent, type ReactNode } from 'react'
import './envSubTabs.css'

export interface EnvSubTabItem<T extends string = string> {
  id: T
  label: string
  panelId: string
  content: ReactNode
}

export interface EnvSubTabsProps<T extends string = string> {
  ariaLabel: string
  value: T
  onChange: (id: T) => void
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
