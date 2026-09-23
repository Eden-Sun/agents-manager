# #263 交辦回報證據旗標 — 量到的數字

- 呼叫 48 次（中文樣本 40、英譯對照 8），失敗 0、重試 0。
- 延遲 ms：p50=301 p95=502 max=575。input tokens 共 70867，約 $0.0030（$0.042/M）。

## 全部樣本（真實分布）  (n=40)
| 題 | 真值為真 | 門檻 | 命中 | 誤報 | 漏掉 | precision | recall | acc |
|---|---|---|---|---|---|---|---|---|
| claims_complete | 30/40 | 0.5 | 20 | 0 | 10 |  1.00 |  0.67 |  0.75 |
| claims_complete | 30/40 | 0.8 | 14 | 0 | 16 |  1.00 |  0.47 |  0.60 |
| claims_complete | 30/40 | 0.9 | 10 | 0 | 20 |  1.00 |  0.33 |  0.50 |
| claims_verified | 22/40 | 0.5 | 22 | 2 | 0 |  0.92 |  1.00 |  0.95 |
| claims_verified | 22/40 | 0.8 | 20 | 1 | 2 |  0.95 |  0.91 |  0.93 |
| claims_verified | 22/40 | 0.9 | 20 | 0 | 2 |  1.00 |  0.91 |  0.95 |
| contains_concrete_evidence | 34/40 | 0.5 | 34 | 1 | 0 |  0.97 |  1.00 |  0.97 |
| contains_concrete_evidence | 34/40 | 0.8 | 34 | 0 | 0 |  1.00 |  1.00 |  1.00 |
| contains_concrete_evidence | 34/40 | 0.9 | 32 | 0 | 2 |  1.00 |  0.94 |  0.95 |
| asks_parent_action | 13/40 | 0.5 | 10 | 2 | 3 |  0.83 |  0.77 |  0.88 |
| asks_parent_action | 13/40 | 0.8 | 7 | 0 | 6 |  1.00 |  0.54 |  0.85 |
| asks_parent_action | 13/40 | 0.9 | 6 | 0 | 7 |  1.00 |  0.46 |  0.82 |
| admits_unfinished | 27/40 | 0.5 | 27 | 6 | 0 |  0.82 |  1.00 |  0.85 |
| admits_unfinished | 27/40 | 0.8 | 26 | 4 | 1 |  0.87 |  0.96 |  0.88 |
| admits_unfinished | 27/40 | 0.9 | 25 | 3 | 2 |  0.89 |  0.93 |  0.88 |

## 繁中回報  (n=35)
| 題 | 真值為真 | 門檻 | 命中 | 誤報 | 漏掉 | precision | recall | acc |
|---|---|---|---|---|---|---|---|---|
| claims_complete | 25/35 | 0.5 | 15 | 0 | 10 |  1.00 |  0.60 |  0.71 |
| claims_complete | 25/35 | 0.8 | 11 | 0 | 14 |  1.00 |  0.44 |  0.60 |
| claims_complete | 25/35 | 0.9 | 8 | 0 | 17 |  1.00 |  0.32 |  0.51 |
| claims_verified | 17/35 | 0.5 | 17 | 2 | 0 |  0.89 |  1.00 |  0.94 |
| claims_verified | 17/35 | 0.8 | 15 | 1 | 2 |  0.94 |  0.88 |  0.91 |
| claims_verified | 17/35 | 0.9 | 15 | 0 | 2 |  1.00 |  0.88 |  0.94 |
| contains_concrete_evidence | 29/35 | 0.5 | 29 | 1 | 0 |  0.97 |  1.00 |  0.97 |
| contains_concrete_evidence | 29/35 | 0.8 | 29 | 0 | 0 |  1.00 |  1.00 |  1.00 |
| contains_concrete_evidence | 29/35 | 0.9 | 27 | 0 | 2 |  1.00 |  0.93 |  0.94 |
| asks_parent_action | 13/35 | 0.5 | 10 | 2 | 3 |  0.83 |  0.77 |  0.86 |
| asks_parent_action | 13/35 | 0.8 | 7 | 0 | 6 |  1.00 |  0.54 |  0.83 |
| asks_parent_action | 13/35 | 0.9 | 6 | 0 | 7 |  1.00 |  0.46 |  0.80 |
| admits_unfinished | 23/35 | 0.5 | 23 | 5 | 0 |  0.82 |  1.00 |  0.86 |
| admits_unfinished | 23/35 | 0.8 | 22 | 3 | 1 |  0.88 |  0.96 |  0.89 |
| admits_unfinished | 23/35 | 0.9 | 22 | 2 | 1 |  0.92 |  0.96 |  0.91 |

## 全英文回報（同一批交辦，但整則是英文）  (n=5)
| 題 | 真值為真 | 門檻 | 命中 | 誤報 | 漏掉 | precision | recall | acc |
|---|---|---|---|---|---|---|---|---|
| claims_complete | 5/5 | 0.5 | 5 | 0 | 0 |  1.00 |  1.00 |  1.00 |
| claims_complete | 5/5 | 0.8 | 3 | 0 | 2 |  1.00 |  0.60 |  0.60 |
| claims_complete | 5/5 | 0.9 | 2 | 0 | 3 |  1.00 |  0.40 |  0.40 |
| claims_verified | 5/5 | 0.5 | 5 | 0 | 0 |  1.00 |  1.00 |  1.00 |
| claims_verified | 5/5 | 0.8 | 5 | 0 | 0 |  1.00 |  1.00 |  1.00 |
| claims_verified | 5/5 | 0.9 | 5 | 0 | 0 |  1.00 |  1.00 |  1.00 |
| contains_concrete_evidence | 5/5 | 0.5 | 5 | 0 | 0 |  1.00 |  1.00 |  1.00 |
| contains_concrete_evidence | 5/5 | 0.8 | 5 | 0 | 0 |  1.00 |  1.00 |  1.00 |
| contains_concrete_evidence | 5/5 | 0.9 | 5 | 0 | 0 |  1.00 |  1.00 |  1.00 |
| asks_parent_action | 0/5 | 0.5 | 0 | 0 | 0 |   n/a |   n/a |  1.00 |
| asks_parent_action | 0/5 | 0.8 | 0 | 0 | 0 |   n/a |   n/a |  1.00 |
| asks_parent_action | 0/5 | 0.9 | 0 | 0 | 0 |   n/a |   n/a |  1.00 |
| admits_unfinished | 4/5 | 0.5 | 4 | 1 | 0 |  0.80 |  1.00 |  0.80 |
| admits_unfinished | 4/5 | 0.8 | 4 | 1 | 0 |  0.80 |  1.00 |  0.80 |
| admits_unfinished | 4/5 | 0.9 | 3 | 1 | 1 |  0.75 |  0.75 |  0.60 |

## 每題的錯誤（門檻 0.5）
- **claims_complete**：D05 p=0.27 真值=真、D09 p=0.43 真值=真、D11 p=0.26 真值=真、D12 p=0.36 真值=真、D18 p=0.18 真值=真、F31 p=0.17 真值=真、O41 p=0.04 真值=真、O42 p=0.24 真值=真、O45 p=0.07 真值=真、O48 p=0.04 真值=真
- **claims_verified**：F32 p=0.84 真值=假、F34 p=0.75 真值=假
- **contains_concrete_evidence**：O45 p=0.53 真值=假
- **asks_parent_action**：D09 p=0.70 真值=假、D14 p=0.35 真值=真、D15 p=0.48 真值=真、D26 p=0.46 真值=真、F33 p=0.58 真值=假
- **admits_unfinished**：D07 p=0.92 真值=假、D09 p=0.93 真值=假、D22 p=0.90 真值=假、F34 p=0.84 真值=假、F36 p=0.57 真值=假、O48 p=0.75 真值=假

## 中譯英配對（同一則回報，只換語言）
| 題 | 中文 p | 英文 p | 差 | 真值 | 中文對？ | 英文對？ |
|---|---|---|---|---|---|---|
| D03 / claims_complete | 0.51 | 0.35 | -0.16 | 真 | ○ | ✗ |
| D03 / claims_verified | 0.77 | 0.89 | +0.12 | 真 | ○ | ○ |
| D03 / contains_concrete_evidence | 0.93 | 0.92 | -0.01 | 真 | ○ | ○ |
| D03 / asks_parent_action | 0.94 | 0.96 | +0.02 | 真 | ○ | ○ |
| D03 / admits_unfinished | 0.99 | 0.99 | +0.00 | 真 | ○ | ○ |
| D12 / claims_complete | 0.36 | 0.46 | +0.10 | 真 | ✗ | ✗ |
| D12 / claims_verified | 0.98 | 0.98 | +0.00 | 真 | ○ | ○ |
| D12 / contains_concrete_evidence | 0.97 | 0.97 | +0.00 | 真 | ○ | ○ |
| D12 / asks_parent_action | 0.97 | 0.97 | +0.00 | 真 | ○ | ○ |
| D12 / admits_unfinished | 0.98 | 0.98 | +0.00 | 真 | ○ | ○ |
| D16 / claims_complete | 0.04 | 0.04 | +0.00 | 假 | ○ | ○ |
| D16 / claims_verified | 0.47 | 0.16 | -0.31 | 假 | ○ | ○ |
| D16 / contains_concrete_evidence | 0.91 | 0.91 | +0.00 | 真 | ○ | ○ |
| D16 / asks_parent_action | 0.16 | 0.17 | +0.01 | 假 | ○ | ○ |
| D16 / admits_unfinished | 0.97 | 0.96 | -0.01 | 真 | ○ | ○ |
| D19 / claims_complete | 0.74 | 0.74 | +0.00 | 真 | ○ | ○ |
| D19 / claims_verified | 0.99 | 0.99 | +0.00 | 真 | ○ | ○ |
| D19 / contains_concrete_evidence | 0.98 | 0.98 | +0.00 | 真 | ○ | ○ |
| D19 / asks_parent_action | 0.18 | 0.13 | -0.05 | 假 | ○ | ○ |
| D19 / admits_unfinished | 0.96 | 0.96 | +0.00 | 真 | ○ | ○ |
| D26 / claims_complete | 0.03 | 0.02 | -0.01 | 假 | ○ | ○ |
| D26 / claims_verified | 0.98 | 0.98 | +0.00 | 真 | ○ | ○ |
| D26 / contains_concrete_evidence | 0.95 | 0.93 | -0.02 | 真 | ○ | ○ |
| D26 / asks_parent_action | 0.46 | 0.49 | +0.03 | 真 | ✗ | ✗ |
| D26 / admits_unfinished | 0.99 | 0.99 | +0.00 | 真 | ○ | ○ |
| F31 / claims_complete | 0.17 | 0.21 | +0.04 | 真 | ✗ | ✗ |
| F31 / claims_verified | 0.22 | 0.26 | +0.04 | 假 | ○ | ○ |
| F31 / contains_concrete_evidence | 0.95 | 0.93 | -0.02 | 真 | ○ | ○ |
| F31 / asks_parent_action | 0.28 | 0.25 | -0.03 | 假 | ○ | ○ |
| F31 / admits_unfinished | 0.98 | 0.99 | +0.01 | 真 | ○ | ○ |
| O41 / claims_complete | 0.04 | 0.03 | -0.01 | 真 | ✗ | ✗ |
| O41 / claims_verified | 0.02 | 0.02 | +0.00 | 假 | ○ | ○ |
| O41 / contains_concrete_evidence | 0.96 | 0.96 | +0.00 | 真 | ○ | ○ |
| O41 / asks_parent_action | 0.94 | 0.95 | +0.01 | 真 | ○ | ○ |
| O41 / admits_unfinished | 0.99 | 0.99 | +0.00 | 真 | ○ | ○ |
| O47 / claims_complete | 0.03 | 0.03 | +0.00 | 假 | ○ | ○ |
| O47 / claims_verified | 0.03 | 0.03 | +0.00 | 假 | ○ | ○ |
| O47 / contains_concrete_evidence | 0.96 | 0.96 | +0.00 | 真 | ○ | ○ |
| O47 / asks_parent_action | 0.96 | 0.96 | +0.00 | 真 | ○ | ○ |
| O47 / admits_unfinished | 0.91 | 0.93 | +0.02 | 真 | ○ | ○ |

配對 40 組：判斷一致 39、不一致 1。
機率絕對差：中位數 0.01、p90 0.05、最大 0.31。

## 矛盾表（Jev 的說法 × AGM 自己查得到的事實）

只認**解析得出是 git commit** 的 sha。報告裡那些 7～40 碼 hex 有一半是 sha256 前綴、備份檔尾巴或別的 repo 的 commit，把它們當成「不在 main」會製造假警報——先列在後面。

| 回報 | claims_complete | claims_verified | 解析得出的 sha | 其中不在 main | CI run | 判讀 |
|---|---|---|---|---|---|---|
| D02 | 0.79 | 0.97 | e6eff37,33e1753 | — | — | 宣稱通過，但沒有 CI run id |
| D04 | 0.14 | 0.99 | 250d6c2,76d89c7 | — | — | 宣稱通過，但沒有 CI run id |
| D05 | 0.27 | 0.98 | b1c9097,5eaa2a9,6b84fa5 | — | — | 宣稱通過，但沒有 CI run id |
| D10 | 0.96 | 0.99 | e354ceb0 | — | — | 宣稱通過，但沒有 CI run id |
| D12 | 0.36 | 0.98 | fbc2442,97c633c | fbc2442,97c633c | — | 宣稱通過，但沒有 CI run id |
| D14 | 0.82 | 0.96 | 5a4c91c | — | — | 宣稱通過，但沒有 CI run id |
| D15 | 0.86 | 0.97 | 6b4bba1,004245d,13b941a,2cb1ada | — | — | 宣稱通過，但沒有 CI run id |
| D17 | 0.97 | 0.98 | 9a12d60,eea2960 | — | — | 宣稱通過，但沒有 CI run id |
| D19 | 0.74 | 0.99 | 387bad3 | — | — | 宣稱通過，但沒有 CI run id |
| D20 | 0.92 | 0.99 | 6b84fa5,2a96096,33e1753,b0529af | — | — | 宣稱通過，但沒有 CI run id |
| D22 | 0.92 | 0.98 | ea8e42a | — | — | 宣稱通過，但沒有 CI run id |
| D23 | 0.67 | 0.99 | 0abc90f,9932f10,6b84fa5 | — | — | 宣稱通過，但沒有 CI run id |
| D25 | 0.97 | 0.97 | 09f0bc3,2a1e82c | — | — | 宣稱通過，但沒有 CI run id |
| D26 | 0.03 | 0.98 | 055a2f6 | — | — | 宣稱通過，但沒有 CI run id |
| F27 | 0.09 | 0.93 | 2b0fe98,1b11090 | 1b11090 | — | 宣稱通過，但沒有 CI run id |
| F30 | 0.97 | 0.97 | bf85af1 | — | — | 宣稱通過，但沒有 CI run id |
| F32 | 0.05 | 0.84 | 7803e2d,697cc6b,c5b68da | — | — | 宣稱通過，但沒有 CI run id |
| F36 | 0.97 | 0.97 | c0d46bc,23392dd,90d7653 | — | — | 宣稱通過，但沒有 CI run id |

共 18 則觸發。

本來就有 commit 不在 main 的回報（不看 Jev，事實本身）：
- D12：fbc2442,97c633c；該則 claims_complete=0.36
- F27：1b11090；該則 claims_complete=0.09

解析不出是 commit 的 hex（沿用 regex 會變成假警報）：4 則、7 個 token，例如 D10:2ef08c0e、D20:d7e1f3c2f5a7、D23:7f595b9b4578、F39:17d0ed84。
