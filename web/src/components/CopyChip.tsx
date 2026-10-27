import { asyncCopyAvailable } from "../lib/copyText"
import { useState } from "react"
import { useT } from "../lib/i18n"
import "./copyChip.css"

/** 「標籤 + 值 + 點一下複製」的小晶片；值是空字串就不渲染（沒有 pane id 的 run 不留空晶片）。 */
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
  if (!asyncCopyAvailable() || done === "fail") {
    return (
      <span className={`copy-chip copy-chip-selectable${className ? " " + className : ""}`} title={title}>
        <span className="copy-chip-label">{label}</span>
        <span className="copy-chip-value mono">{value}</span>
        <span className="copy-chip-hint">{done === "fail" ? t("複製失敗，長按選取") : t("長按選取後複製")}</span>
      </span>
    )
  }
  return (
    <button
      type="button"
      className={`copy-chip${done === "ok" ? " done" : ""}${className ? " " + className : ""}`}
      title={`${title}\n${value}${t("（點擊複製）")}`}
      onClick={() => {
        void navigator.clipboard.writeText(value).then(() => {
          setDone("ok")
          setTimeout(() => setDone(null), 1200)
        }, () => setDone("fail"))
      }}
    >
      <span className="copy-chip-label">{label}</span>
      <span className="copy-chip-value mono">{value}</span>
      {done === "ok" ? <span className="copy-chip-ok">{t("已複製")}</span> : null}
    </button>
  )
}
