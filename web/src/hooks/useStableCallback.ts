import { useCallback, useLayoutEffect, useRef } from 'react'

/**
 * 身分永遠不變、呼叫時永遠跑**最新一次 render** 的那個函式。給傳進 `memo` 元件的回呼用：
 * 每次 render 重新宣告的函式會讓 `memo` 的淺比較永遠失敗；用 ref 轉一手，元件看到的是同一個函式，
 * 裡面讀的 state 仍然是最新的（不會拿到過期的閉包）。
 */
export function useStableCallback<A extends unknown[], R>(fn: (...args: A) => R): (...args: A) => R {
  const ref = useRef(fn)
  useLayoutEffect(() => {
    ref.current = fn
  })
  return useCallback((...args: A) => ref.current(...args), [])
}
