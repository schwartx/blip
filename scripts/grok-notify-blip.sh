#!/usr/bin/env bash
# Grok → host blipd (WSL networkingMode=mirrored → 127.0.0.1:7788).
#
# Wire-up (same pattern as scripts/codex-notify-blip.sh):
#   ~/.grok/hooks/blip.json          → lifecycle Stop / Notification / StopFailure
#   ~/.grok/config.toml              → [[ui.notifications.hooks]] (env fallback)
#
# Two entry points (same script):
#   1) Lifecycle hooks — JSON on stdin
#   2) ui.notifications.hooks — env GROK_EVENT / GROK_MESSAGE / GROK_SESSION_ID
#
# Fail-open. Depends: bash, jaq, curl.

set -u

PATH="/home/linuxbrew/.linuxbrew/bin:/usr/local/bin:/usr/bin:/bin${PATH:+:$PATH}"

BLIP_URL="${BLIP_URL:-http://127.0.0.1:7788/notify}"
TIMEOUT="${BLIP_TIMEOUT:-2}"

command -v jaq >/dev/null 2>&1 || exit 0

# Prefer stdin (lifecycle hook). Env path is the fallback for ui.notifications.
if [[ -t 0 ]]; then
  stdin=""
else
  stdin="$(cat || true)"
fi

title="Grok"
body=""
level="normal"
id=""
source="grok"

if [[ -n "$stdin" ]]; then
  # Lifecycle envelope (camelCase). Ignore non-turn Stop fires.
  parsed="$(
    printf '%s' "$stdin" | jaq -c '
      . as $e
      | ($e.hookEventName // $e.hook_event_name // "") as $ev
      | ($e.reason // "") as $reason
      | if ($ev == "stop" or $ev == "Stop") and $reason != "" and $reason != "end_turn"
        then empty
        else
          ("Grok · " + (
            ($e.cwd // $e.workspaceRoot // "")
            | split("/") | map(select(length > 0)) | last // "未知项目"
          )) as $title
          | (($e.lastAssistantMessage // $e.message // $e.last_assistant_message // "")
              | if type == "string" then . else "" end) as $raw
          | (if ($raw | length) > 200 then $raw[0:197] + "…" else $raw end) as $body
          | (if $ev == "notification" or $ev == "Notification"
               or $ev == "stop_failure" or $ev == "StopFailure"
             then "critical"
             else "normal"
             end) as $level
          | (if ($e.sessionId // $e.session_id // "") != ""
             then "grok-" + ($e.sessionId // $e.session_id)
             else null
             end) as $id
          | {
              title: $title,
              body: (if $body == "" then
                       if $ev == "stop_failure" or $ev == "StopFailure" then "API 错误，回合中断"
                       elif $ev == "notification" or $ev == "Notification" then "需要你的确认"
                       else "回合结束"
                       end
                     else $body end),
              level: $level,
              source: "grok",
              id: $id
            }
          | with_entries(select(.value != null))
        end
    ' 2>/dev/null || true
  )"
  [[ -n "${parsed:-}" ]] || exit 0
  json="$parsed"
else
  event="${GROK_EVENT:-}"
  message="${GROK_MESSAGE:-}"
  session="${GROK_SESSION_ID:-}"

  cwd="${PWD:-}"
  if [[ -n "$cwd" && "$cwd" != "/" ]]; then
    title="Grok · ${cwd##*/}"
  else
    title="Grok · 未知项目"
  fi

  case "$event" in
    approval_required)
      level="critical"
      [[ -z "$message" ]] && message="需要你的确认"
      ;;
    agent_error)
      level="critical"
      [[ -z "$message" ]] && message="出错了"
      ;;
    turn_complete)
      level="normal"
      [[ -z "$message" ]] && message="回合结束"
      ;;
    task_complete)
      level="normal"
      [[ -z "$message" ]] && message="任务完成"
      ;;
    session_ready)
      level="low"
      [[ -z "$message" ]] && message="会话就绪"
      ;;
    *)
      level="normal"
      [[ -z "$message" ]] && message="${event:-通知}"
      ;;
  esac

  id=""
  [[ -n "$session" ]] && id="grok-${session}"

  json="$(
    jaq -nc \
      --arg title "$title" \
      --arg body "$message" \
      --arg level "$level" \
      --arg id "$id" \
      --arg source "$source" \
      '{
        title: $title,
        body: (if $body == "" then null else $body end),
        level: $level,
        id: (if $id == "" then null else $id end),
        source: $source
      } | with_entries(select(.value != null))'
  )" || exit 0
fi

curl -sS -m "$TIMEOUT" --noproxy '*' \
  -X POST "$BLIP_URL" \
  -H 'Content-Type: application/json' \
  -d "$json" \
  >/dev/null 2>&1 || true

exit 0
