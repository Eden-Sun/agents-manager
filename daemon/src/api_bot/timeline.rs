//! claude-api bot 每回合的可觀測性時間軸與花費拆分（`GET /api/bots/{id}/turns/{turn_id}/api-timeline`）。
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct BreakdownCategory {
    pub tokens: u64,
    pub cost_usd: f64,
    pub percentage: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LargestItem {
    pub category: String,
    pub label: String,
    pub cost_usd: f64,
    pub percentage: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CostBreakdown {
    pub input: BreakdownCategory,
    pub cache_read: BreakdownCategory,
    pub cache_write: BreakdownCategory,
    pub output: BreakdownCategory,
    pub compaction: BreakdownCategory,
    pub server_tool: BreakdownCategory,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TurnSummary {
    pub total_cost_usd: f64,
    pub breakdown: CostBreakdown,
    pub largest_item: LargestItem,
    pub suggestions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TimelineToolItem {
    #[serde(rename = "type")]
    pub kind: String, // "tool"
    pub name: String,
    pub duration_ms: Option<u64>,
    pub is_error: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TimelineRequestItem {
    #[serde(rename = "type")]
    pub kind: String, // "request"
    pub ordinal: i64,
    pub timestamp: String,
    pub iteration_type: String, // "message" | "compaction"
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub stop_reason: Option<String>,
    pub duration_ms: Option<u64>,
    pub speed: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TimelineTurnEndItem {
    #[serde(rename = "type")]
    pub kind: String, // "turn_end"
    pub timestamp: Option<String>,
    pub status: String,
    pub halt_reason: String,
}

/// 模型費率（單位：0.1 µUSD/token，即 $1/M tokens = 10 tenths）。
struct Rates {
    input: f64,
    write_5m: f64,
    write_1h: f64,
    read: f64,
    output: f64,
}

fn model_rates(model: &str) -> Rates {
    match model {
        "claude-fable-5-2" => Rates {
            input: 80.0,
            write_5m: 100.0,
            write_1h: 160.0,
            read: 2.0,
            output: 400.0,
        },
        _ => Rates {
            // claude-opus-5-5 or default
            input: 40.0,
            write_5m: 50.0,
            write_1h: 80.0,
            read: 2.0,
            output: 200.0,
        },
    }
}

/// 嘗試取得單次回合的時間軸與「這回合為什麼花了 $X」拆分分析。
pub async fn get_turn_timeline(
    db: &SqlitePool,
    bot_id: &str,
    turn_id: &str,
) -> Result<Option<Value>, sqlx::Error> {
    // 1. 驗證 turn 是否存在且屬於該 bot
    type TurnRow = (
        String,         // t.id
        String,         // t.status
        String,         // t.created_at
        Option<String>, // t.completed_at
        Option<String>, // t.start_error
        String,         // b.model
        bool,           // b.fast
        i64,            // t.cache_ttl_secs
    );
    let turn_opt: Option<TurnRow> = sqlx::query_as(
        "SELECT t.id, t.status, t.created_at, t.completed_at, t.start_error, b.model, b.fast, t.cache_ttl_secs \
         FROM turns t \
         JOIN conversations c ON c.id = t.conversation_id \
         JOIN bots b ON b.id = c.bot_id \
         WHERE b.id = ? AND t.id = ?",
    )
    .bind(bot_id)
    .bind(turn_id)
    .fetch_optional(db)
    .await?;

    let Some(turn) = turn_opt else {
        return Ok(None);
    };

    let (
        t_id,
        t_status,
        t_created_at,
        t_completed_at,
        t_start_error,
        b_model,
        b_fast,
        t_cache_ttl_secs,
    ) = turn;

    // 2. 撈取該 turn 的所有 attempts
    type AttemptRow = (
        String,         // id
        Option<String>, // session_id
        i64,            // ordinal
        String,         // requested_speed
        Option<String>, // actual_speed
        String,         // state
        Option<String>, // usage_json
        i64,            // reserved_microusd
        Option<i64>,    // estimated_microusd
        String,         // created_at
        Option<String>, // finished_at
    );

    let attempts: Vec<AttemptRow> = sqlx::query_as(
        "SELECT id, session_id, ordinal, requested_speed, actual_speed, state, usage_json, \
                reserved_microusd, estimated_microusd, created_at, finished_at \
         FROM api_bot_attempts \
         WHERE bot_id = ? AND turn_id = ? \
         ORDER BY ordinal ASC, created_at ASC",
    )
    .bind(bot_id)
    .bind(turn_id)
    .fetch_all(db)
    .await?;

    // 3. 嘗試讀取 session checkpoint_json 作為備援（提取 tools 與 stop_reason）
    let session_id_opt = attempts.first().and_then(|a| a.1.clone());
    let checkpoint_val: Option<Value> = if let Some(sid) = session_id_opt {
        let raw_ckpt: Option<String> = sqlx::query_scalar(
            "SELECT checkpoint_json FROM api_bot_sessions WHERE id = ? AND bot_id = ?",
        )
        .bind(&sid)
        .bind(bot_id)
        .fetch_optional(db)
        .await?;
        raw_ckpt.and_then(|s| serde_json::from_str(&s).ok())
    } else {
        None
    };

    // 4. 計算回合總耗時（ms）
    let turn_duration_ms: Option<u64> = match (&t_completed_at, &t_created_at) {
        (Some(completed), created) => {
            if let (Some(c), Some(s)) = (
                crate::db::parse_ts(completed),
                crate::db::parse_ts(created),
            ) {
                let ms = (c - s).num_milliseconds();
                if ms >= 0 {
                    Some(ms as u64)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    };

    let result = build_timeline_and_summary(
        bot_id,
        &t_id,
        &t_status,
        &t_created_at,
        t_completed_at.as_deref(),
        t_start_error.as_deref(),
        &b_model,
        b_fast,
        t_cache_ttl_secs,
        turn_duration_ms,
        &attempts,
        checkpoint_val.as_ref(),
    );

    Ok(Some(result))
}

/// 核心純函式邏輯：建置時間軸、花費拆分、最大項與可省建議。
fn build_timeline_and_summary(
    bot_id: &str,
    turn_id: &str,
    turn_status: &str,
    turn_created_at: &str,
    turn_completed_at: Option<&str>,
    turn_start_error: Option<&str>,
    model: &str,
    bot_fast: bool,
    cache_ttl_secs: i64,
    turn_duration_ms: Option<u64>,
    attempts: &[(
        String,         // id
        Option<String>, // session_id
        i64,            // ordinal
        String,         // requested_speed
        Option<String>, // actual_speed
        String,         // state
        Option<String>, // usage_json
        i64,            // reserved_microusd
        Option<i64>,    // estimated_microusd
        String,         // created_at
        Option<String>, // finished_at
    )],
    checkpoint: Option<&Value>,
) -> Value {
    let rates = model_rates(model);
    let is_ttl_1h = cache_ttl_secs > 300;

    let mut total_cost_usd = 0.0_f64;
    let mut input_tokens = 0_u64;
    let mut input_cost_usd = 0.0_f64;
    let mut cache_read_tokens = 0_u64;
    let mut cache_read_cost_usd = 0.0_f64;
    let mut cache_write_tokens = 0_u64;
    let mut cache_write_cost_usd = 0.0_f64;
    let mut output_tokens = 0_u64;
    let mut output_cost_usd = 0.0_f64;
    let mut compaction_tokens = 0_u64;
    let mut compaction_cost_usd = 0.0_f64;
    let mut server_tool_tokens = 0_u64;
    let mut server_tool_cost_usd = 0.0_f64;

    let mut timeline_items: Vec<Value> = Vec::new();
    let mut last_stop_reason: Option<String> = None;
    let mut fast_mode_used = false;

    // 從 checkpoint 取得 responses 與 messages（備援用）
    let empty_vec = Vec::new();
    let responses = checkpoint
        .and_then(|c| c.get("responses"))
        .and_then(Value::as_array)
        .unwrap_or(&empty_vec);
    let messages = checkpoint
        .and_then(|c| c.get("messages"))
        .and_then(Value::as_array)
        .unwrap_or(&empty_vec);

    for (att_idx, att) in attempts.iter().enumerate() {
        let (
            _id,
            _session_id,
            ordinal,
            req_speed,
            act_speed,
            state,
            usage_raw,
            _reserved_microusd,
            estimated_microusd,
            created_at,
            finished_at,
        ) = att;

        let effective_speed = act_speed.as_deref().unwrap_or(req_speed.as_str());
        let is_fast = effective_speed == "fast" || bot_fast;
        if is_fast {
            fast_mode_used = true;
        }
        let speed_factor = if is_fast { 2.0 } else { 1.0 };

        let attempt_cost = estimated_microusd.unwrap_or(0) as f64 / 1_000_000.0;
        if state == "recorded" {
            total_cost_usd += attempt_cost;
        }

        let usage_obj: Option<Value> = usage_raw
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());

        // 計算 request 耗時
        let req_duration_ms: Option<u64> = usage_obj
            .as_ref()
            .and_then(|u| u.get("duration_ms"))
            .and_then(Value::as_u64)
            .or_else(|| match (finished_at, created_at) {
                (Some(f), c) => {
                    if let (Some(ft), Some(ct)) =
                        (crate::db::parse_ts(f), crate::db::parse_ts(c))
                    {
                        let ms = (ft - ct).num_milliseconds();
                        if ms >= 0 {
                            Some(ms as u64)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            });

        // 判定 stop_reason
        let resp_fallback = responses.get(*ordinal as usize);
        let resp_stop_reason = resp_fallback
            .and_then(|r| r.get("stop_reason"))
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let req_stop_reason = usage_obj
            .as_ref()
            .and_then(|u| u.get("stop_reason"))
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .or(resp_stop_reason);

        if let Some(r) = &req_stop_reason {
            last_stop_reason = Some(r.clone());
        }

        // 解析 iterations 或頂層 tokens
        let iterations = usage_obj
            .as_ref()
            .and_then(|u| u.get("iterations"))
            .and_then(Value::as_array);

        if let Some(iters) = iterations.filter(|a| !a.is_empty()) {
            // 有 iterations（通常代表包含 compaction 輪次）
            for it in iters {
                let it_type = it
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                let it_in = it
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let it_out = it
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let it_read = it
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let it_write = it
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);

                let write_rate = if is_ttl_1h {
                    rates.write_1h
                } else {
                    rates.write_5m
                };
                let it_cost = (it_in as f64 * rates.input
                    + it_write as f64 * write_rate
                    + it_read as f64 * rates.read
                    + it_out as f64 * rates.output)
                    * speed_factor
                    / 10_000_000.0;

                if it_type == "compaction" {
                    compaction_tokens += it_in + it_out + it_read + it_write;
                    compaction_cost_usd += it_cost;
                } else {
                    input_tokens += it_in;
                    input_cost_usd += (it_in as f64 * rates.input) * speed_factor / 10_000_000.0;
                    output_tokens += it_out;
                    output_cost_usd +=
                        (it_out as f64 * rates.output) * speed_factor / 10_000_000.0;
                    cache_read_tokens += it_read;
                    cache_read_cost_usd +=
                        (it_read as f64 * rates.read) * speed_factor / 10_000_000.0;
                    cache_write_tokens += it_write;
                    cache_write_cost_usd +=
                        (it_write as f64 * write_rate) * speed_factor / 10_000_000.0;
                }

                timeline_items.push(json!({
                    "type": "request",
                    "ordinal": ordinal,
                    "timestamp": created_at,
                    "iteration_type": it_type,
                    "input_tokens": it_in,
                    "cache_read_tokens": it_read,
                    "cache_write_tokens": it_write,
                    "output_tokens": it_out,
                    "cost_usd": it_cost,
                    "stop_reason": req_stop_reason,
                    "duration_ms": req_duration_ms,
                    "speed": effective_speed,
                }));
            }
        } else {
            // 標準 message 請求（無 iterations）
            let u_in = usage_obj
                .as_ref()
                .and_then(|u| u.get("input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let u_out = usage_obj
                .as_ref()
                .and_then(|u| u.get("output_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let u_read = usage_obj
                .as_ref()
                .and_then(|u| u.get("cache_read_input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let u_write = usage_obj
                .as_ref()
                .and_then(|u| u.get("cache_creation_input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);

            // server tool check
            let u_server_tool = usage_obj
                .as_ref()
                .and_then(|u| u.get("server_tool_use"))
                .is_some();

            let write_rate = if is_ttl_1h {
                rates.write_1h
            } else {
                rates.write_5m
            };

            let c_in = (u_in as f64 * rates.input) * speed_factor / 10_000_000.0;
            let c_read = (u_read as f64 * rates.read) * speed_factor / 10_000_000.0;
            let c_write = (u_write as f64 * write_rate) * speed_factor / 10_000_000.0;
            let c_out = (u_out as f64 * rates.output) * speed_factor / 10_000_000.0;

            input_tokens += u_in;
            input_cost_usd += c_in;
            cache_read_tokens += u_read;
            cache_read_cost_usd += c_read;
            cache_write_tokens += u_write;
            cache_write_cost_usd += c_write;
            output_tokens += u_out;
            output_cost_usd += c_out;

            if u_server_tool {
                server_tool_tokens += 1;
                // server tool 額外標記
                server_tool_cost_usd += 0.0;
            }

            timeline_items.push(json!({
                "type": "request",
                "ordinal": ordinal,
                "timestamp": created_at,
                "iteration_type": "message",
                "input_tokens": u_in,
                "cache_read_tokens": u_read,
                "cache_write_tokens": u_write,
                "output_tokens": u_out,
                "cost_usd": attempt_cost,
                "stop_reason": req_stop_reason,
                "duration_ms": req_duration_ms,
                "speed": effective_speed,
            }));
        }

        // 解析此 request 呼叫並執行的工具列表
        // 優先讀取 usage_json["tools"]，次選從 checkpoint 提取
        let recorded_tools = usage_obj
            .as_ref()
            .and_then(|u| u.get("tools"))
            .and_then(Value::as_array);

        if let Some(tool_list) = recorded_tools {
            for t in tool_list {
                let name = t
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let dur = t.get("duration_ms").and_then(Value::as_u64);
                let is_err = t.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                timeline_items.push(json!({
                    "type": "tool",
                    "name": name,
                    "duration_ms": dur,
                    "is_error": is_err,
                }));
            }
        } else if let Some(resp) = resp_fallback {
            // 從 response content 抓 tool_use
            let content_blocks = resp
                .get("content")
                .and_then(Value::as_array)
                .unwrap_or(&empty_vec);
            for b in content_blocks {
                if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let tool_id = b.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = b
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();

                    // 在 messages 搜尋對應的 tool_result
                    let is_error = messages.iter().any(|m| {
                        m.get("content")
                            .and_then(Value::as_array)
                            .map(|arr| {
                                arr.iter().any(|blk| {
                                    blk.get("type").and_then(Value::as_str)
                                        == Some("tool_result")
                                        && blk.get("tool_use_id").and_then(Value::as_str)
                                            == Some(tool_id)
                                        && blk.get("is_error").and_then(Value::as_bool)
                                            == Some(true)
                                })
                            })
                            .unwrap_or(false)
                    });

                    timeline_items.push(json!({
                        "type": "tool",
                        "name": name,
                        "duration_ms": Value::Null,
                        "is_error": is_error,
                    }));
                }
            }
        }

        // 若最後一個 attempt 且無後續工具
        if att_idx == attempts.len() - 1 && req_stop_reason.is_some() {
            last_stop_reason = req_stop_reason;
        }
    }

    // 5. 判定回合結束原因
    let halt_reason = if let Some(err) = turn_start_error {
        err.to_string()
    } else if let Some(reason) = last_stop_reason {
        reason
    } else if turn_status == "completed" {
        "end_turn".to_string()
    } else {
        turn_status.to_string()
    };

    timeline_items.push(json!({
        "type": "turn_end",
        "timestamp": turn_completed_at,
        "status": turn_status,
        "halt_reason": halt_reason,
    }));

    // 6. 計算各類別佔比與最大項
    // 若 calculated items sum > 0 且 total_cost_usd == 0（例如只有 mock 數據），以 sum 為準
    let calculated_sum = input_cost_usd
        + cache_read_cost_usd
        + cache_write_cost_usd
        + output_cost_usd
        + compaction_cost_usd
        + server_tool_cost_usd;

    let base_cost = if total_cost_usd > 0.0 {
        total_cost_usd
    } else {
        calculated_sum
    };

    let calc_pct = |cost: f64| -> f64 {
        if base_cost > 0.0 {
            ((cost / base_cost) * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        }
    };

    let breakdown = CostBreakdown {
        input: BreakdownCategory {
            tokens: input_tokens,
            cost_usd: input_cost_usd,
            percentage: calc_pct(input_cost_usd),
        },
        cache_read: BreakdownCategory {
            tokens: cache_read_tokens,
            cost_usd: cache_read_cost_usd,
            percentage: calc_pct(cache_read_cost_usd),
        },
        cache_write: BreakdownCategory {
            tokens: cache_write_tokens,
            cost_usd: cache_write_cost_usd,
            percentage: calc_pct(cache_write_cost_usd),
        },
        output: BreakdownCategory {
            tokens: output_tokens,
            cost_usd: output_cost_usd,
            percentage: calc_pct(output_cost_usd),
        },
        compaction: BreakdownCategory {
            tokens: compaction_tokens,
            cost_usd: compaction_cost_usd,
            percentage: calc_pct(compaction_cost_usd),
        },
        server_tool: BreakdownCategory {
            tokens: server_tool_tokens,
            cost_usd: server_tool_cost_usd,
            percentage: calc_pct(server_tool_cost_usd),
        },
    };

    // 找出最大項
    let categories: [(&str, &str, &BreakdownCategory); 6] = [
        ("cache_write", "快取寫入", &breakdown.cache_write),
        ("input", "一般輸入", &breakdown.input),
        ("output", "輸出", &breakdown.output),
        ("cache_read", "快取讀取", &breakdown.cache_read),
        ("compaction", "對話壓縮", &breakdown.compaction),
        ("server_tool", "伺服器工具", &breakdown.server_tool),
    ];

    let mut largest = LargestItem {
        category: "input".into(),
        label: "一般輸入".into(),
        cost_usd: 0.0,
        percentage: 0.0,
    };
    for (cat_key, cat_label, cat_val) in categories {
        if cat_val.cost_usd > largest.cost_usd
            || (largest.cost_usd == 0.0 && cat_val.tokens > 0)
        {
            largest = LargestItem {
                category: cat_key.into(),
                label: cat_label.into(),
                cost_usd: cat_val.cost_usd,
                percentage: cat_val.percentage,
            };
        }
    }

    // 7. 產生可省建議 (suggestions)
    let mut suggestions: Vec<String> = Vec::new();

    // 建議一：快取冷掉重寫
    if breakdown.cache_write.tokens > 0 && breakdown.cache_write.cost_usd > 0.0 {
        let speed_factor = if fast_mode_used { 2.0 } else { 1.0 };
        let hit_cost = (breakdown.cache_write.tokens as f64 * rates.read) * speed_factor
            / 10_000_000.0;
        let savings = (breakdown.cache_write.cost_usd - hit_cost).max(0.0);
        if savings >= 0.0005 {
            let pct = ((savings / breakdown.cache_write.cost_usd) * 100.0).round();
            suggestions.push(format!(
                "快取冷掉重寫花費 ${:.3}；若在快取保留期（{}）內送出，快取讀取僅需 ${:.3}，可省約 ${:.3}（省 {:.0}%）。",
                breakdown.cache_write.cost_usd,
                if is_ttl_1h { "1 小時" } else { "5 分鐘" },
                hit_cost,
                savings,
                pct
            ));
        }
    }

    // 建議二：fast 模式費用提示
    if fast_mode_used && base_cost > 0.001 {
        let fast_savings = base_cost * 0.5;
        suggestions.push(format!(
            "本回合使用了 fast 模式（2× 費率），若切換為 standard 模式可省下約 ${:.3}（省 50%）。",
            fast_savings
        ));
    }

    // 建議三：輸出（含思考）費用過高
    if largest.category == "output"
        && breakdown.output.cost_usd >= 0.005
        && breakdown.output.percentage >= 40.0
    {
        suggestions.push(format!(
            "輸出（含深度思考）佔了本回合 {:.1}% 的花費（${:.3}）。若任務不需要極深層推理，可考慮調低思考強度（thinking effort）以降低輸出 token 費用。",
            largest.percentage, breakdown.output.cost_usd
        ));
    }

    // 建議四：對話壓縮
    if breakdown.compaction.cost_usd >= 0.001 {
        suggestions.push(format!(
            "對話累積過長觸發了伺服器端壓縮（花費 ${:.3}）。建議開新對話或精簡上下文，避免重複壓縮歷史。",
            breakdown.compaction.cost_usd
        ));
    }

    // 建議五：請求次數過多
    if attempts.len() >= 4 {
        suggestions.push(format!(
            "本回合進行了 {} 次 API 請求與工具來回。若將多個工具操作合併或批次處理，可大幅減少 API 請求次數與重複上下文傳輸成本。",
            attempts.len()
        ));
    }

    if suggestions.is_empty() {
        suggestions.push("本回合花費結構合理，快取與用量配置皆屬良好。".to_string());
    }

    let summary = TurnSummary {
        total_cost_usd: base_cost,
        breakdown,
        largest_item: largest,
        suggestions,
    };

    json!({
        "bot_id": bot_id,
        "turn_id": turn_id,
        "status": turn_status,
        "halt_reason": halt_reason,
        "total_cost_usd": base_cost,
        "duration_ms": turn_duration_ms,
        "created_at": turn_created_at,
        "completed_at": turn_completed_at,
        "summary": summary,
        "timeline": timeline_items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_turn_summary_does_not_panic() {
        let res = build_timeline_and_summary(
            "b1",
            "t1",
            "queued",
            "2026-11-15T01:00:00.000Z",
            None,
            None,
            "claude-opus-5-5",
            false,
            300,
            None,
            &[],
            None,
        );

        assert_eq!(res["bot_id"], "b1");
        assert_eq!(res["turn_id"], "t1");
        assert_eq!(res["status"], "queued");
        assert_eq!(res["total_cost_usd"], 0.0);
        let timeline = res["timeline"].as_array().unwrap();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0]["type"], "turn_end");
        assert_eq!(timeline[0]["status"], "queued");
    }

    #[test]
    fn test_breakdown_with_compaction_and_message_iterations() {
        let usage = json!({
            "duration_ms": 1500,
            "stop_reason": "end_turn",
            "iterations": [
                {
                    "type": "compaction",
                    "input_tokens": 50,
                    "cache_creation_input_tokens": 100_000,
                    "cache_read_input_tokens": 0,
                    "output_tokens": 4000
                },
                {
                    "type": "message",
                    "input_tokens": 10,
                    "cache_creation_input_tokens": 1000,
                    "cache_read_input_tokens": 20_000,
                    "output_tokens": 500
                }
            ]
        });

        let attempts = vec![(
            "att-1".to_string(),
            Some("sess-1".to_string()),
            0,
            "standard".to_string(),
            Some("standard".to_string()),
            "recorded".to_string(),
            Some(serde_json::to_string(&usage).unwrap()),
            10_000,
            Some(58_544), // $0.058544
            "2026-11-15T02:00:00.000Z".to_string(),
            Some("2026-11-15T02:00:01.500Z".to_string()),
        )];

        let res = build_timeline_and_summary(
            "b-comp",
            "t-comp",
            "completed",
            "2026-11-15T02:00:00.000Z",
            Some("2026-11-15T02:00:02.000Z"),
            None,
            "claude-opus-5-5",
            false,
            300,
            Some(2000),
            &attempts,
            None,
        );

        let summary = &res["summary"];
        let breakdown = &summary["breakdown"];
        assert!(breakdown["compaction"]["tokens"].as_u64().unwrap() > 0);
        assert!(breakdown["compaction"]["cost_usd"].as_f64().unwrap() > 0.0);
        assert!(breakdown["input"]["tokens"].as_u64().unwrap() > 0);
        assert!(breakdown["cache_read"]["tokens"].as_u64().unwrap() > 0);
        assert!(breakdown["cache_write"]["tokens"].as_u64().unwrap() > 0);
        assert!(breakdown["output"]["tokens"].as_u64().unwrap() > 0);

        // timeline 應有 2 個 request（compaction + message）與 1 個 turn_end
        let timeline = res["timeline"].as_array().unwrap();
        assert_eq!(timeline.len(), 3);
        assert_eq!(timeline[0]["type"], "request");
        assert_eq!(timeline[0]["iteration_type"], "compaction");
        assert_eq!(timeline[1]["type"], "request");
        assert_eq!(timeline[1]["iteration_type"], "message");
        assert_eq!(timeline[2]["type"], "turn_end");
    }

    #[test]
    fn test_cache_miss_rewrite_suggestion_generated() {
        let usage = json!({
            "input_tokens": 100,
            "output_tokens": 100,
            "cache_creation_input_tokens": 50_000, // 冷掉重寫 50k
            "cache_read_input_tokens": 0,
            "duration_ms": 2000,
            "stop_reason": "end_turn"
        });

        let attempts = vec![(
            "att-cold".to_string(),
            None,
            0,
            "standard".to_string(),
            None,
            "recorded".to_string(),
            Some(serde_json::to_string(&usage).unwrap()),
            50_000,
            Some(25_240), // ~$0.025
            "2026-11-15T03:00:00.000Z".to_string(),
            Some("2026-11-15T03:00:02.000Z".to_string()),
        )];

        let res = build_timeline_and_summary(
            "b-cold",
            "t-cold",
            "completed",
            "2026-11-15T03:00:00.000Z",
            Some("2026-11-15T03:00:02.000Z"),
            None,
            "claude-opus-5-5",
            false,
            300,
            Some(2000),
            &attempts,
            None,
        );

        let largest = &res["summary"]["largest_item"];
        assert_eq!(largest["category"], "cache_write");
        assert_eq!(largest["label"], "快取寫入");

        let suggestions = res["summary"]["suggestions"].as_array().unwrap();
        assert!(suggestions
            .iter()
            .any(|s| s.as_str().unwrap().contains("快取冷掉重寫花費")));
    }

    #[test]
    fn test_tools_timeline_with_recorded_tools_and_fallback() {
        // 情況 A：usage_json 內建 tools
        let usage_with_tools = json!({
            "input_tokens": 200,
            "output_tokens": 50,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 1000,
            "stop_reason": "tool_use",
            "duration_ms": 1100,
            "tools": [
                { "name": "bash", "duration_ms": 320, "is_error": false },
                { "name": "read_file", "duration_ms": 15, "is_error": true }
            ]
        });

        let attempts = vec![(
            "att-tools".to_string(),
            None,
            0,
            "standard".to_string(),
            None,
            "recorded".to_string(),
            Some(serde_json::to_string(&usage_with_tools).unwrap()),
            10_000,
            Some(1_100),
            "2026-11-15T04:00:00.000Z".to_string(),
            Some("2026-11-15T04:00:01.100Z".to_string()),
        )];

        let res = build_timeline_and_summary(
            "b-tools",
            "t-tools",
            "completed",
            "2026-11-15T04:00:00.000Z",
            Some("2026-11-15T04:00:02.000Z"),
            None,
            "claude-opus-5-5",
            false,
            300,
            Some(2000),
            &attempts,
            None,
        );

        let timeline = res["timeline"].as_array().unwrap();
        // 1 request + 2 tools + 1 turn_end = 4
        assert_eq!(timeline.len(), 4);
        assert_eq!(timeline[0]["type"], "request");
        assert_eq!(timeline[1]["type"], "tool");
        assert_eq!(timeline[1]["name"], "bash");
        assert_eq!(timeline[1]["duration_ms"], 320);
        assert_eq!(timeline[1]["is_error"], false);
        assert_eq!(timeline[2]["type"], "tool");
        assert_eq!(timeline[2]["name"], "read_file");
        assert_eq!(timeline[2]["duration_ms"], 15);
        assert_eq!(timeline[2]["is_error"], true);
        assert_eq!(timeline[3]["type"], "turn_end");
    }

    #[test]
    fn test_tools_timeline_fallback_from_checkpoint() {
        // 情況 B：舊資料 usage_json 沒有 tools，從 checkpoint 提取
        let usage_plain = json!({
            "input_tokens": 150,
            "output_tokens": 40,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 500,
            "stop_reason": "tool_use",
        });

        let attempts = vec![(
            "att-old".to_string(),
            Some("sess-old".to_string()),
            0,
            "standard".to_string(),
            None,
            "recorded".to_string(),
            Some(serde_json::to_string(&usage_plain).unwrap()),
            5_000,
            Some(1_000),
            "2026-11-15T05:00:00.000Z".to_string(),
            Some("2026-11-15T05:00:01.000Z".to_string()),
        )];

        let checkpoint = json!({
            "responses": [
                {
                    "stop_reason": "tool_use",
                    "content": [
                        { "type": "text", "text": "let me search" },
                        { "type": "tool_use", "id": "tu_1", "name": "search_code" }
                    ]
                }
            ],
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "tool_result", "tool_use_id": "tu_1", "content": "found 1 result", "is_error": false }
                    ]
                }
            ]
        });

        let res = build_timeline_and_summary(
            "b-old",
            "t-old",
            "completed",
            "2026-11-15T05:00:00.000Z",
            Some("2026-11-15T05:00:02.000Z"),
            None,
            "claude-opus-5-5",
            false,
            300,
            Some(2000),
            &attempts,
            Some(&checkpoint),
        );

        let timeline = res["timeline"].as_array().unwrap();
        assert_eq!(timeline.len(), 3); // 1 request + 1 tool + 1 turn_end
        assert_eq!(timeline[1]["type"], "tool");
        assert_eq!(timeline[1]["name"], "search_code");
        assert!(timeline[1]["duration_ms"].is_null()); // fallback 無精確 duration
        assert_eq!(timeline[1]["is_error"], false);
    }
}
