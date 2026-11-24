#!/bin/sh
input=$(cat)

# Account: first 5 chars of the logged-in Gmail/email address
_cfg_dir="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
if [ "$_cfg_dir" = "$HOME/.claude" ]; then
  _cfg_json="$HOME/.claude.json"
else
  _cfg_json="$_cfg_dir/.claude.json"
fi
account=$(jq -r '.oauthAccount.emailAddress // empty' "$_cfg_json" 2>/dev/null | cut -c1-5)
[ -z "$account" ] && account="?"
# Current directory: basename of workspace.current_dir (fallback to .cwd)
cur_dir=$(echo "$input" | jq -r '.workspace.current_dir // .cwd // empty')
[ -n "$cur_dir" ] && cur_dir=$(basename "$cur_dir")

model=$(echo "$input" | jq -r '.model.display_name // .model.id // "unknown"')
# Shorten model to 3-letter prefix + version (case-insensitive)
_model_lower=$(echo "$model" | tr '[:upper:]' '[:lower:]')
_ver=$(echo "$model" | grep -oE '[0-9]+(\.[0-9]+)?' | head -1)
case "$_model_lower" in
  *opus*)   _prefix="OP" ;;
  *sonnet*) _prefix="SON" ;;
  *haiku*)  _prefix="HAI" ;;
  *)        _prefix="" ;;
esac
if [ -n "$_prefix" ]; then
  if [ -n "$_ver" ]; then
    model="${_prefix}${_ver}"
  else
    model="${_prefix}"
  fi
fi

# Reasoning effort (low / medium / high / xhigh). A child pane that another agent forked
# carries its own --effort, and nothing else on screen says which one it got, so show it
# next to the model. Older CLIs do not send the field: then it is simply left out.
effort=$(echo "$input" | jq -r '.effort.level // empty')
case "$effort" in
  low)    eff="L" ;;
  medium) eff="M" ;;
  high)   eff="H" ;;
  xhigh)  eff="XH" ;;
  "")     eff="" ;;
  *)      eff=$(echo "$effort" | cut -c1-2 | tr '[:lower:]' '[:upper:]') ;;
esac

# 5-hour limit
five_pct=$(echo "$input" | jq -r '.rate_limits.five_hour.used_percentage // empty')
five_resets_at=$(echo "$input" | jq -r '.rate_limits.five_hour.resets_at // empty')

# 7-day limit
week_pct=$(echo "$input" | jq -r '.rate_limits.seven_day.used_percentage // empty')
week_resets_at=$(echo "$input" | jq -r '.rate_limits.seven_day.resets_at // empty')

# Compute reset countdown string: "Xd Yh" or "Yh Zm" or "now"
_reset_str() {
  _epoch="$1"
  [ -z "$_epoch" ] && return
  _now=$(date +%s)
  _diff=$(( _epoch - _now ))
  if [ "$_diff" -le 0 ]; then
    echo "now"
  else
    _days=$(( _diff / 86400 ))
    _hours=$(( (_diff % 86400) / 3600 ))
    _mins=$(( (_diff % 3600) / 60 ))
    if [ "$_days" -gt 0 ]; then
      echo "${_days}d ${_hours}h"
    else
      echo "${_hours}h ${_mins}m"
    fi
  fi
}

five_reset_str=$(_reset_str "$five_resets_at")
week_reset_str=$(_reset_str "$week_resets_at")

# Context window usage — shown inline with the model, e.g. "OP5 44%"
ctx_pct=$(echo "$input" | jq -r '.context_window.used_percentage // empty')

# Build output: gmail[:5] | MODEL ctx% | 5h limit | 7d limit
model_field="$model"
[ -n "$eff" ] && model_field="${model_field} ${eff}"
if [ -n "$ctx_pct" ]; then
  ctx_str=$(echo "$ctx_pct" | awk '{printf "%.0f", $1}')
  model_field="${model_field} ctx ${ctx_str}%"
fi
out="${account}"
[ -n "$cur_dir" ] && out="${out} | ${cur_dir}"
out="${out} | ${model_field}"
if [ -n "$five_pct" ]; then
  five_left=$(echo "$five_pct" | awk '{printf "%.0f", 100 - $1}')
  if [ -n "$five_reset_str" ]; then
    out="${out} | 5h left ${five_left}%(rst ${five_reset_str})"
  else
    out="${out} | 5h left ${five_left}%"
  fi
else
  out="${out} | 5h:-"
fi
if [ -n "$week_pct" ]; then
  week_left=$(echo "$week_pct" | awk '{printf "%.0f", 100 - $1}')
  if [ -n "$week_reset_str" ]; then
    out="${out} | 7d left ${week_left}%(rst ${week_reset_str})"
  else
    out="${out} | 7d left ${week_left}%"
  fi
else
  out="${out} | 7d:-"
fi

# --- Fable quota (Max plan only, shown as remaining %, same idiom as 5h/7d) ---
# Plan (Max/Pro/...) is cached in the config dir since it does not change between renders.
_fable_cache_file="${_cfg_dir}/.statusline-fable-cache"
_fable_cache_ttl=86400   # 24h
_now_epoch=$(date +%s 2>/dev/null)

_is_max=""
if [ -f "$_fable_cache_file" ]; then
  _cache_ts=$(sed -n '1p' "$_fable_cache_file" 2>/dev/null)
  _cache_max=$(sed -n '2p' "$_fable_cache_file" 2>/dev/null)
  if [ -n "$_cache_ts" ] && [ -n "$_now_epoch" ] && [ $(( _now_epoch - _cache_ts )) -lt "$_fable_cache_ttl" ] 2>/dev/null; then
    _is_max="$_cache_max"
  fi
fi

# Only touch the keychain/network if plan is unknown (needs profile lookup) or already known Max (needs usage lookup)
if [ -z "$_is_max" ] || [ "$_is_max" = "1" ]; then
  _keychain_service="Claude Code-credentials"
  if [ "$_cfg_dir" != "$HOME/.claude" ]; then
    _cfg_hash=$(printf '%s' "$_cfg_dir" | shasum -a 256 2>/dev/null | cut -c1-8)
    [ -n "$_cfg_hash" ] && _keychain_service="Claude Code-credentials-${_cfg_hash}"
  fi
  if command -v security >/dev/null 2>&1; then
    _creds_json=$(security find-generic-password -s "$_keychain_service" -w 2>/dev/null)
  else
    _creds_json=$(cat "$_cfg_dir/.credentials.json" 2>/dev/null)
  fi
  _access_token=$(echo "$_creds_json" | jq -r '.claudeAiOauth.accessToken // empty' 2>/dev/null)

  if [ -z "$_is_max" ] && [ -n "$_access_token" ]; then
    _profile_json=$(curl -s --max-time 2 \
      -H "Authorization: Bearer ${_access_token}" \
      -H "anthropic-beta: oauth-2025-04-20" \
      "https://api.anthropic.com/api/oauth/profile" 2>/dev/null)
    _has_max=$(echo "$_profile_json" | jq -r '.account.has_claude_max // empty' 2>/dev/null)
    if [ "$_has_max" = "true" ]; then
      _is_max="1"
    elif [ "$_has_max" = "false" ]; then
      _is_max="0"
    fi
    if [ "$_is_max" = "1" ] || [ "$_is_max" = "0" ]; then
      printf "%s\n%s\n" "$_now_epoch" "$_is_max" > "$_fable_cache_file" 2>/dev/null
    fi
  fi

  # Fable usage: the endpoint rate-limits fast and every open session renders this script, so
  # at most one render per config dir hits it (mkdir lock), the cache is long-lived, and a 429
  # parks everyone until Retry-After. 5h/7d come free from the statusLine input; only Fable needs this.
  if [ "$_is_max" = "1" ] && [ -n "$_access_token" ]; then
    _fable_usage_cache_file="${_cfg_dir}/.statusline-fable-usage-cache"
    _fable_usage_backoff_file="${_cfg_dir}/.statusline-fable-usage-backoff"   # epoch: no network call before this
    _fable_usage_lock_dir="${_cfg_dir}/.statusline-fable-usage.lock"
    _fable_usage_fresh_ttl=600    # serve from cache within this window; no network call at all
    _fable_usage_stale_ttl=3600   # on a failed/skipped refresh, keep showing the last good value up to this age

    _cached_usage_ts=""
    _cached_fable_pct=""
    if [ -f "$_fable_usage_cache_file" ]; then
      _cached_usage_ts=$(sed -n '1p' "$_fable_usage_cache_file" 2>/dev/null)
      _cached_fable_pct=$(sed -n '2p' "$_fable_usage_cache_file" 2>/dev/null)
    fi
    _usage_age=""
    if [ -n "$_cached_usage_ts" ] && [ -n "$_now_epoch" ]; then
      _usage_age=$(( _now_epoch - _cached_usage_ts )) 2>/dev/null
    fi
    _backoff_until=$(cat "$_fable_usage_backoff_file" 2>/dev/null)
    case "$_backoff_until" in ''|*[!0-9]*) _backoff_until=0 ;; esac

    # A lock left behind by a killed render must not block refreshes forever.
    if [ -d "$_fable_usage_lock_dir" ] && [ -n "$(find "$_fable_usage_lock_dir" -maxdepth 0 -mmin +1 2>/dev/null)" ]; then
      rmdir "$_fable_usage_lock_dir" 2>/dev/null
    fi

    if [ -n "$_usage_age" ] && [ "$_usage_age" -ge 0 ] 2>/dev/null && [ "$_usage_age" -lt "$_fable_usage_fresh_ttl" ] 2>/dev/null; then
      # Fresh cache: use it, no network call.
      fable_pct="$_cached_fable_pct"
    elif [ "$_now_epoch" -lt "$_backoff_until" ] 2>/dev/null || ! mkdir "$_fable_usage_lock_dir" 2>/dev/null; then
      # Rate-limited, or another session is refreshing right now: show the last good value.
      if [ -n "$_usage_age" ] && [ "$_usage_age" -ge 0 ] 2>/dev/null && [ "$_usage_age" -lt "$_fable_usage_stale_ttl" ] 2>/dev/null; then
        fable_pct="$_cached_fable_pct"
      fi
    else
      # We hold the lock: refresh. Explicitly capture the HTTP status so a 429/error body
      # (which is still valid JSON) can never be mistaken for a real 200 payload.
      _usage_hdrs=$(mktemp 2>/dev/null)
      _usage_resp=$(curl -s --max-time 2 -w '\n%{http_code}' -D "${_usage_hdrs:-/dev/null}" \
        -H "Authorization: Bearer ${_access_token}" \
        -H "anthropic-beta: oauth-2025-04-20" \
        "https://api.anthropic.com/api/oauth/usage" 2>/dev/null)
      _usage_http_code=$(printf '%s' "$_usage_resp" | tail -n1)
      _usage_json=$(printf '%s' "$_usage_resp" | sed '$d')

      _fresh_pct=""
      if [ "$_usage_http_code" = "200" ]; then
        _has_error=$(echo "$_usage_json" | jq -r 'if type == "object" and has("error") then "1" else empty end' 2>/dev/null)
        if [ -z "$_has_error" ]; then
          _fresh_pct=$(echo "$_usage_json" | jq -r '[.limits[]? | select((.scope.model.display_name // "") | ascii_downcase == "fable")][0].percent // empty' 2>/dev/null)
          # Only accept a plain, in-range number — anything else (null/blank/garbage) is treated as unavailable.
          case "$_fresh_pct" in
            ''|*[!0-9.]*) _fresh_pct="" ;;
          esac
        fi
        # Answered but no Fable row (or an error body): asking again on the next render won't change that.
        [ -z "$_fresh_pct" ] && printf "%s\n" "$(( _now_epoch + _fable_usage_fresh_ttl ))" > "$_fable_usage_backoff_file" 2>/dev/null
      elif [ "$_usage_http_code" = "429" ]; then
        # Honour Retry-After (seconds), but never retry sooner than 5 minutes.
        _retry_after=$(tr -d '\r' < "${_usage_hdrs:-/dev/null}" 2>/dev/null | awk -F': *' 'tolower($1)=="retry-after"{print $2; exit}')
        case "$_retry_after" in ''|*[!0-9]*) _retry_after=0 ;; esac
        [ "$_retry_after" -lt 300 ] && _retry_after=300
        printf "%s\n" "$(( _now_epoch + _retry_after ))" > "$_fable_usage_backoff_file" 2>/dev/null
      else
        # Timeout / 5xx / expired token: back off a minute rather than retrying on every render.
        printf "%s\n" "$(( _now_epoch + 60 ))" > "$_fable_usage_backoff_file" 2>/dev/null
      fi
      [ -n "$_usage_hdrs" ] && rm -f "$_usage_hdrs"

      if [ -n "$_fresh_pct" ]; then
        fable_pct="$_fresh_pct"
        printf "%s\n%s\n" "$_now_epoch" "$_fresh_pct" > "$_fable_usage_cache_file.tmp.$$" 2>/dev/null \
          && mv -f "$_fable_usage_cache_file.tmp.$$" "$_fable_usage_cache_file" 2>/dev/null
      elif [ -n "$_usage_age" ] && [ "$_usage_age" -ge 0 ] 2>/dev/null && [ "$_usage_age" -lt "$_fable_usage_stale_ttl" ] 2>/dev/null; then
        # Refresh failed — keep showing the last good value instead of flickering,
        # but only while it isn't too stale. A bad/error response never overwrites the cache file.
        fable_pct="$_cached_fable_pct"
      fi
      rmdir "$_fable_usage_lock_dir" 2>/dev/null
    fi
  fi

  if [ -n "$fable_pct" ]; then
    fable_left=$(echo "$fable_pct" | awk '{v=100-$1; if (v<0) v=0; if (v>100) v=100; printf "%.0f", v}' 2>/dev/null)
    [ -n "$fable_left" ] && out="${out} | F5 left ${fable_left}%"
  fi
fi

printf "%s" "$out"
