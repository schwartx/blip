#!/usr/bin/env bash

# Codex appends one JSON event argument to the configured notify command.
if [[ $# -ne 1 ]]; then
    exit 0
fi

blip_url="${BLIP_URL:-http://127.0.0.1:7788}"
endpoint="${blip_url%/}/notify"

# shellcheck disable=SC2016 # $message belongs to jaq, not Bash.
payload="$(printf '%s' "$1" | jaq --compact-output '
    select(.type == "agent-turn-complete")
    | ((."last-assistant-message" // "") as $message
       | {
           title: (
             "Codex · " + (
               (.cwd // "")
               | split("/")
               | map(select(length > 0))
               | last // "未知项目"
             )
           ),
           body: (
             if ($message | type) == "string" and ($message | length) > 0
             then if ($message | length) > 2000
                  then $message[:1997] + "..."
                  else $message
                  end
             else "任务已完成"
             end
           ),
           level: "normal",
           source: "codex"
         }
       + if ((."thread-id" // "") | type) == "string" and ((."thread-id" // "") | length) > 0
         then {id: ("codex-" + ."thread-id")}
         else {}
         end)
' 2>/dev/null || true)"

[[ -n "$payload" ]] || exit 0

# Notification delivery is best-effort and must never disrupt Codex.
curl --silent \
    --output /dev/null \
    --max-time 3 \
    --noproxy '*' \
    --header 'Content-Type: application/json' \
    --data-binary "$payload" \
    "$endpoint" >/dev/null 2>&1 || true

exit 0
