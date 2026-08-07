# blip

A cursor-anchored, draggable, always-on-top notification panel for Windows.

Built because Windows toast — and therefore BurntToast — gets four things wrong
for personal use:

| Toast does | blip does |
|---|---|
| Always bottom-right | Appears where you're already looking (at the cursor); drag it once and it stays there |
| Stacks a card per event | One list. Same `--id` updates a row in place; identical content collapses to `×N` |
| Close ✕ is the most prominent control | Content is the focus; click a row to dismiss it; one big "都看过了" button clears everything |
| Expires on wall-clock, even while you're away | A row's countdown only runs while it is actually on screen and unattended |

```
Rust · Win32 · DirectComposition · Direct2D · DirectWrite
resident daemon + thin CLI + HTTP
```

---

## Quick start

```bash
cargo build --release

target/release/blip.exe "构建完成"
```

That's it. The daemon isn't running yet, so the CLI starts it, waits for the
pipe, and delivers — about 200ms cold, single-digit milliseconds after that.
You never launch `blipd.exe` yourself.

---

## CLI

```bash
blip "构建完成"
blip -t "部署失败" -b "3 个健康检查未通过" -l critical
blip -t "编译中" --id build --progress 60          # updates in place
blip --dismiss build                                # withdraw early
blip --clear                                        # clear + hide

cargo test 2>&1 | blip --stdin -t "测试输出"
some-long-task; blip --exit-code $LASTEXITCODE "任务结束"
```

`--exit-code` picks the level for you: `0` → normal, anything else → critical.

| Flag | Meaning |
|---|---|
| `-t, --title` | Headline (also the first positional arg) |
| `-b, --body` | Secondary line, wrapped and dimmed |
| `-l, --level` | `low` / `normal` / `critical` |
| `--id` | Same id replaces that row instead of adding one |
| `-s, --source` | Origin tag |
| `--ttl <s>` | Override lifetime; `0` = never expires |
| `--sticky` | `--ttl 0` |
| `--progress 0-100` | Progress bar; suppresses re-popping |
| `--action <cmd>` | Shell command run when the row is clicked |
| `--stdin` | Read the body from stdin |

---

## HTTP

Loopback only by default. `blip --config` writes a documented config file.

```bash
# The one-liner that matters: anything that speaks HTTP can use this.
curl -d "构建完成" http://127.0.0.1:7788/notify

curl -X POST http://127.0.0.1:7788/notify \
  -H 'Content-Type: application/json' \
  -d '{"title":"部署失败","body":"v2.3.1","level":"critical","id":"deploy"}'

curl -X DELETE http://127.0.0.1:7788/notify/deploy
curl http://127.0.0.1:7788/health
```

A body without `Content-Type: application/json` is taken as the title, first
line becoming the headline and the rest the body. That fallback is deliberate —
GitHub Actions, Grafana, Home Assistant, n8n and iOS Shortcuts all become
zero-adaptation senders.

**Exposing it to the LAN requires a token.** Set `bind = "0.0.0.0:7788"` and the
daemon refuses to start unless `token` is non-empty, because a topmost window
anyone on the network can push content into is a genuinely useful attack surface.
Send it as `X-Token:` or `Authorization: Bearer`.

---

## Behaviour

**Position.** The panel opens near the cursor — the only place on screen you're
guaranteed to already be looking. It flips at screen edges like a context menu,
clamps to the work area (never under the taskbar), and is per-monitor DPI aware.
It will never open on top of the cursor hotspot, because a window that
materialises under the pointer eats the click you were about to make.

**Drag to pin.** Dragging is the user saying "put it here", so it's also the
gesture that stops the panel following the cursor. Tray → double-click resets to
cursor mode. No setting to find.

**Dismissing.** Click anywhere on a row to dismiss it (running its `--action`
first, if it has one). The per-row ✕ appears on hover and drops the row *without*
firing its action. The footer button clears everything and hides in one go —
clearing without hiding would just leave an empty panel sitting there.

**Expiry.** A row counts down only when the panel is visible, the row is inside
the scrolled viewport, the pointer isn't resting on the panel, and you've used
the keyboard or mouse in the last 30 seconds. Walk away mid-build and the result
is still there when you come back.

**Focus.** The panel is `WS_EX_NOACTIVATE` — it can appear while you're typing
and you won't lose a keystroke or drop an IME composition. Esc is registered as a
hotkey *only while the pointer is over the panel*, so it never steals Esc from
the app you're actually working in.

**Idle.** After 90 seconds with nothing to show, the D3D/D2D/DComp stack is
released and the working set collapses. The process, the pipe and the HTTP
listener stay up; the next notification pays a one-off rebuild instead of every
notification paying a process spawn.

---

## Architecture

```
blip.exe   (console)   parse args → named pipe → exit          ~8ms
                              │ spawns if absent
blipd.exe  (windows)   ┌──────┴─────────────────────────┐
                       │ pipe thread ─┐                 │
                       │ http thread ─┼→ mpsc → PostMessage
                       │              │                 │
                       │       message loop ── store ── layout ── D2D/DComp
                       └────────────────────────────────┘
```

Three transports, one `Command` type, one policy engine.

**Why not a Windows service:** services run in Session 0 and are physically
unable to draw on the user's desktop. Anything that shows UI has to be a
per-session process.

**Why the swapchain is fixed-size:** it's allocated once at the panel's maximum
extent. Content changes move the *window*, and DComp clips the surface to it —
resizing a swapchain mid-animation flickers.

| File | |
|---|---|
| `src/model.rs` | Wire protocol + runtime notification |
| `src/store.rs` | List, same-id update, TTL state machine |
| `src/config.rs` | TOML config with working defaults |
| `src/ipc/pipe.rs` | Named pipe; also the single-instance lock |
| `src/ipc/http.rs` | Hand-rolled HTTP, no async runtime |
| `src/ui/layout.rs` | Geometry — shared by renderer and hit-tester |
| `src/ui/render.rs` | D3D11 → composition swapchain → D2D → DComp |
| `src/ui/window.rs` | Window, message loop, drag, hover, show/hide |
| `src/ui/position.rs` | Cursor anchor, multi-monitor DPI, edge flip |

Logic that can be tested without a GPU, is: `cargo test` covers the TTL pause
rules, id collapsing, eviction, edge flipping, cursor avoidance, and the
invariant that the hit-tester agrees with what was drawn.

---

## Config

`%APPDATA%\blip\config.toml`, created by `blip --config`. Every field has a
working default; a malformed file falls back to defaults and reports itself as a
critical notification rather than refusing to start.

```toml
bind = "127.0.0.1:7788"
token = ""
max_items = 50
max_visible_rows = 5
width = 340.0
font = "Microsoft YaHei UI"

[levels]
low_ttl = 4.0
normal_ttl = 7.0
critical_ttl = 0.0     # 0 = never expires
low_pops = false       # low lands in the list without opening the panel

[behavior]
cursor_gap = 18.0
drag_to_pin = true
idle_release = 90.0
```

---

## Autostart

Optional — the CLI starts the daemon on demand. Task Scheduler beats the `Run`
key because it supports a startup delay, which keeps you out of the login disk
storm:

```
schtasks /create /tn "blip" /tr "\"C:\path\to\blipd.exe\"" /sc onlogon /delay 0000:30 /f
```

---

## Known gaps

- Rows scroll but there's no inertia.
- No history panel; cleared is cleared. That boundary is deliberate — the moment
  it grows read/unread state and search it becomes the notification centre this
  was built to get away from.
- `--source` is carried end to end but not yet used for grouping or muting.
- Panel can't cover exclusive-fullscreen D3D or the UAC secure desktop. Nothing
  can, without a signed `uiAccess` binary.
