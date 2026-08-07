# blip — Claude Code plugin

Claude Code stops and waits for you more often than you'd like: a permission
prompt mid-task, or the turn ending while you're in another window. This plugin
puts those two moments in [blip](https://github.com/schwartx/blip)'s panel — at
the cursor, one row per session, dismissed by clicking anywhere on the row.

```
/plugin marketplace add schwartx/blip
/plugin install blip@blip
```

Requires blip **0.2.0 or newer** running (`blip --version`). If blip isn't
running, nothing happens and nothing complains — the hook fails silently and
your session is unaffected.

## What it hooks

| Event | Level | Behaviour |
|---|---|---|
| `Notification` / `permission_prompt` — Claude is blocked on your approval | `critical` | Pops immediately, never expires, breaks through full-screen games |
| `Stop` — the turn ended | `normal` | Pops **only if you've been away 15s**; otherwise lands in the list quietly |
| `StopFailure` — the turn died on an API error | `critical` | Always pops, never expires |

`StopFailure` gets no `if_idle`, and that isn't an oversight. The argument for
suppressing `Stop` is "if you were sitting there, you already know" — which is
false here. A turn killed by a rate limit or an overloaded server looks, from
the terminal, a lot like a turn still thinking. You want to know either way.

The row's headline is the **project directory name**, because with several
sessions open that's the only thing that answers "which one wants me?". The
`session_id` becomes the row id, so a session that asks three times updates one
row instead of stacking three.

`Stop` also carries `last_assistant_message`, so the body is Claude's closing
line — the panel tells you *what happened*, not just that something did.

## Why `Stop` and not `idle_prompt`

`idle_prompt` fires about a minute after a turn ends, and only if you never came
back. That's the right *policy* and the wrong *mechanism*: a minute is a long
time to sit unaware that your turn is done, and the payload is a fixed string
with nothing in it.

`Stop` fires the instant the turn ends and says what was said. The "only if you
weren't watching" part is then blip's job — it asks Windows how long the
keyboard and mouse have been idle (`if_idle=15` in the URL) and skips the pop if
you were right there. A notification telling you what you just watched happen is
how a notifier trains you to ignore it.

Tune it by editing the `if_idle` seconds in `hooks/hooks.json`, or drop the
parameter entirely to pop on every turn.

## No script, no dependencies

The whole plugin is `hooks/hooks.json`. Claude Code's `http` handler POSTs the
event JSON straight to blip, so there is no shell, no interpreter, no `PATH`
entry, no file encoding, and no process spawn on a hook that fires precisely
when you're already waiting.

That is also why the mapping from hook payload to notification lives in blip
(`src/ipc/hook.rs`) rather than here: a plugin that had to compute anything
would need a script, and a script would need an interpreter this plugin can't
assume you have. `if_idle` is the sharper case — it needs `GetLastInputInfo`
and there is no script that can usefully ask that question on behalf of a
window it doesn't own.

## If blip isn't at the default address

`hooks.json` targets `127.0.0.1:7788`. Claude Code substitutes `${user_config.*}`
into hook *commands* but not into an `http` handler's `url`, so a configurable
address would mean shipping a script and taking on an interpreter dependency —
which costs every user something to serve a few. If your `config.toml` says
otherwise, or blip runs on another machine on your LAN, skip the plugin and put
the same hooks in your own `settings.json` with the right address.

## Turning it off

```
/plugin uninstall blip@blip
```
