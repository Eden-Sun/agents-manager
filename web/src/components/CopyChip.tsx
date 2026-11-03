import { copyText } from "../lib/copyText"
import { useState } from "react"
import { useT } from "../lib/i18n"
import "./copyChip.css"

/**
 * 「標籤 + 值 + 點一下複製」的小晶片；值是空字串就不渲染（沒有 pane id 的 run 不留空晶片）。
 * 值已經在手上，點擊當下同步複製（`copyText` 先走 execCommand），http 的 LAN 位址也能複製；真的失敗才改成可選取文字。
 */
export function CopyChip({
  label,
  value,
  title,
  className,
}: {
  label: string
  value: string
  title: string
  /** 額外的 class（目前只有 `desk-only`：手機的副標題列放不下這麼多晶片）。 */
  className?: string
}) {
  const t = useT()
  const [done, setDone] = useState<"ok" | "fail" | null>(null)
  if (!value) return null
  if (done === "fail") {
    return (
      <span className={`copy-chip copy-chip-selectable${className ? " " + className : ""}`} title={title}>
        <span className="copy-chip-label">{label}</span>
        <span className="copy-chip-value mono">{value}</span>
        <span className="copy-chip-hint">{t("複製失敗，長按選取")}</span>
      </span>
    )
  }
  return (
    <button
      type="button"
      className={`copy-chip${done === "ok" ? " done" : ""}${className ? " " + className : ""}`}
      title={`${title}\n${value}${t("（點擊複製）")}`}
      onClick={() => {
        // 同步呼叫（不 await 在前）：execCommand('copy') 只在點擊手勢當下有效。
        void copyText(value).then((ok) => {
          if (!ok) return setDone("fail")
          setDone("ok")
          setTimeout(() => setDone(null), 1200)
        })
      }}
    >
      <span className="copy-chip-label">{label}</span>
      <span className="copy-chip-value mono">{value}</span>
      {done === "ok" ? <span className="copy-chip-ok">{t("已複製")}</span> : null}
    </button>
  )
}
