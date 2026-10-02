# blip

一个锚定在光标位置、可拖拽、始终置顶的通知面板，支持 Windows 和 macOS 11+。

Windows 使用原有的 Win32 界面；macOS 使用原生 AppKit 面板和菜单栏图标。两者共享通知协议、列表状态、CLI、HTTP 和 Claude Code hook。Linux 可以运行共享核心的测试，但没有桌面通知界面。

之所以做这个，是因为 Windows 系统通知（以及基于它的 BurntToast）在个人使用场景下有四个地方做得不对：

| 系统通知的做法 | blip 的做法 |
|---|---|
| 永远出现在右下角 | 出现在你正在看的地方（光标附近）；拖动一次后就固定在那里 |
| 每条通知堆一张卡片 | 只有一个列表。相同的 `--id` 会原地更新那一行；内容相同的通知会折叠成 `×N` |
| 关闭按钮 ✕ 是最显眼的控件 | 内容才是重点；点击整行即可关闭；一个大大的「都看过了」按钮一键清空 |
| 按挂钟时间过期，哪怕你不在电脑前 | 只有当这一行真的显示在屏幕上、且你没有操作时，倒计时才会走 |

```
Rust · Windows: Win32 / DirectComposition / Direct2D / DirectWrite
     · macOS: AppKit / Core Graphics
常驻守护进程 + 轻量 CLI + HTTP
```

---

## 快速开始

### Windows

```bash
cargo build --release

target/release/blip.exe "构建完成"
```

就这么简单。守护进程此时还没启动，CLI 会自动拉起它、等待管道就绪、再投递通知——冷启动大约 200ms，之后每次都是个位数毫秒。你不需要自己去启动 `blipd.exe`。

### macOS

需要 macOS 11 或更新版本、Rust，以及 Xcode Command Line Tools（`xcode-select --install`）。在 Mac 上构建：

```bash
cargo test --locked
./scripts/package-macos.sh
```

脚本生成 `dist/blip-<版本>-macos-arm64.zip`（Apple Silicon）或 `dist/blip-<版本>-macos-x86_64.zip`（Intel）。解压后得到 `Blip.app`，其中同时包含守护进程和 CLI；应用不会出现在 Dock 中。安装到个人应用目录并把 CLI 加入当前 shell 的 PATH：

```bash
mkdir -p "$HOME/Applications" "$HOME/.local/bin"
# 替换为构建出的 zip 路径；安装前先用 blip --quit 停止旧实例。
ditto -x -k dist/blip-0.2.0-macos-arm64.zip "$HOME/Applications"
ln -s "$HOME/Applications/Blip.app/Contents/MacOS/blip" "$HOME/.local/bin/blip"
export PATH="$HOME/.local/bin:$PATH"
blip "构建完成"
```

若已有同名 `blip` 链接，请先确认它指向的位置，再自行更新。要让 PATH 在新终端中生效，可把上面的 `export` 加入 `~/.zshrc`。CLI 会通过 Launch Services 启动应用，使用用户专属的 Unix domain socket 投递通知；也可以直接 `open "$HOME/Applications/Blip.app"` 打开菜单栏应用。

要在 Mac 上构建另一种 CPU 的包，先用 `rustup target add aarch64-apple-darwin` 或 `rustup target add x86_64-apple-darwin` 安装对应目标，再给脚本传入 `--target <目标>`。当前产物按架构分别打包，没有生成 universal binary。

开发时可以直接构建并运行两个二进制，不必打包；CLI 冷启动需要旁边已有 `blipd`：

```bash
cargo build --locked --bins
target/debug/blip "构建完成"
```

macOS 上的 `--action` 使用 `/bin/sh -c`，因此 PowerShell 或 `cmd.exe` 命令需要改为对应的 shell 命令。例如：

```bash
cargo test --locked; blip --exit-code "$?" "测试结束"
blip -t "查看项目" --action 'open https://github.com/schwartx/blip'
```

这些构建没有 Developer ID 签名和公证，适合本机开发验证。GitHub CI 的 macOS zip 也属于开发构建；公开分发前需要用 Developer ID 签名，并通过 Apple 公证与 stapling。下载的未公证应用可能会被 Gatekeeper 拦截，当前 Windows 发布工作流不会发布 macOS 安装包。

### macOS 界面与验证

macOS 面板显示标题、正文、重复次数和进度，来源可通过行的悬浮提示查看。点击通知行可关闭；带 `--action` 的行会先执行命令，与 Windows 的行为一致。拖动标题栏可固定位置，菜单栏提供显示/隐藏、清空、恢复跟随光标、手动安静模式、配置和退出操作。TTL 在隐藏、悬停、滚动到不可见区域、超过 30 秒无输入或系统睡眠/会话不活跃时暂停。

在实际 Mac 上还需要人工验证这些系统交互；普通 CI 构建和核心测试不能代替它们：

1. 在编辑器输入中文时发送通知，确认面板出现后光标和输入法仍留在编辑器。
2. 用 `--id build --progress 25` 再发送 `--id build --progress 100`，确认只更新同一行；测试点击关闭、带 `--action` 的行和清空。
3. 拖拽固定，使用菜单恢复跟随，再在不同显示器、缩放比例和屏幕边缘发送通知。
4. 设置 `--ttl 5`，检查悬停、隐藏、离开电脑、锁屏/睡眠时倒计时暂停，返回后恢复。
5. 从菜单开启安静模式，检查 normal 通知留在列表、critical 通知仍弹出；检查全屏应用、Spaces 切换和退出后 CLI 自动重启。

macOS 目前使用手动安静模式，没有读取系统 Focus/勿扰状态；也没有 Windows 的全局 Esc 快捷键、D3D 渲染资源释放或应用内登录项开关。全屏应用和 Spaces 的窗口层级、输入法及多屏行为仍需上述真机验证。

---

## CLI

```bash
blip "构建完成"
blip -t "部署失败" -b "3 个健康检查未通过" -l critical
blip -t "编译中" --id build --progress 60          # 原地更新
blip --dismiss build                                # 提前撤回
blip --clear                                        # 清空并隐藏
blip --quit                                         # 停止守护进程（不会顺带启动一个）

cargo test 2>&1 | blip --stdin -t "测试输出"
some-long-task; blip --exit-code $LASTEXITCODE "任务结束"
```

最后一行是 PowerShell 写法；macOS 的 shell 使用 `blip --exit-code "$?" "任务结束"`。

`--exit-code` 会自动选择级别：`0` → normal，其他任何值 → critical。

| 参数 | 含义 |
|---|---|
| `-t, --title` | 标题（也可以作为第一个位置参数） |
| `-b, --body` | 副标题，会自动换行并变暗显示 |
| `-l, --level` | `low` / `normal` / `critical` |
| `--id` | 相同 id 会替换该行，而不是新增一行 |
| `-s, --source` | 来源标签 |
| `--ttl <s>` | 覆盖生存时间；`0` = 永不过期 |
| `--sticky` | 等价于 `--ttl 0` |
| `--progress 0-100` | 进度条；同时会抑制重复弹出 |
| `--if-idle <s>` | 只有在你离开超过这个时长后才弹出，否则悄悄进入列表 |
| `--action <cmd>` | 点击该行时执行的 shell 命令 |
| `--stdin` | 从标准输入读取正文 |

---

## HTTP

始终开启，并且默认在你的局域网内可访问。`blip --config` 会写出一份配置文件。

```bash
# 最重要的一行：任何会说 HTTP 的东西都能用这个。
curl -d "构建完成" http://127.0.0.1:7788/notify

curl -X POST http://127.0.0.1:7788/notify \
  -H 'Content-Type: application/json' \
  -d '{"title":"部署失败","body":"v2.3.1","level":"critical","id":"deploy"}'

curl -X DELETE http://127.0.0.1:7788/notify/deploy
curl http://127.0.0.1:7788/health
```

如果请求体没有带 `Content-Type: application/json`，就会被当作标题处理：第一行作为标题，其余作为正文。这个兜底逻辑是刻意设计的——这样 GitHub Actions、Grafana、Home Assistant、n8n 和 iOS 快捷指令都能零适配直接接入。

### Claude Code

`POST /hook/claude` 直接读取 [Claude Code](https://code.claude.com/docs/en/hooks) 的 hook 载荷，因此配套插件只需要一份 `hooks.json`，别无其他——不需要脚本、不需要解释器、不需要配置 `PATH`，也不需要在恰好是你已经在等待的那个 hook 上额外启动一个进程。

```
/plugin marketplace add schwartx/blip
/plugin install blip@blip
```

权限确认请求会以 `critical` 级别立即弹出。一轮对话结束会以 `normal` 级别附带 `?if_idle=15` 送达，也就是**只有在你 15 秒内没有碰过键盘或鼠标时才会弹出**——否则这一行会悄悄进入列表。`Stop` 事件无论你是否在看都会触发，因为一个打断你、告诉你刚刚已经读过的内容的面板，恰恰是训练你去忽略它的方式。无论哪种情况，通知都不会丢失，只是弹出与否是有条件的。

因 API 错误（限流、服务过载）而中断的一轮对话会触发 `StopFailure`，它以 `critical` 级别送达，且不带任何 `if_idle`。「你已经知道了」这个理由在这里不成立：单看终端，一轮已经挂掉的对话和一轮还在思考的对话看起来几乎一样。

标题是项目目录——这是唯一能区分两个并发会话的信息——而 `session_id` 会被用作这一行的 id，因此同一个会话连续询问三次也只会更新同一行，而不是堆成三行。`Stop` 事件会携带 `last_assistant_message`，所以正文就是 Claude 的结束语。

`level` 和 `if_idle` 都放在查询字符串里，因为它们是*事件*本身的属性，而不是载荷的属性：载荷里甚至不会说明是哪条匹配规则选中了它。而且 `if_idle` 只有拥有窗口的一方才能回答——Windows 的 `GetLastInputInfo` 或 macOS 的系统输入空闲时间都由守护进程查询，这也是这层映射逻辑放在 `src/ipc/hook.rs` 里最清楚的理由。

**没有任何身份验证，且 `bind` 默认是 `0.0.0.0:7788`。** 这正是 HTTP 传输方式存在的意义——一台构建机、一台 NAS，或者你另一台机器上的 Claude Code 会话，不需要任何人先去改配置文件，就能直接连到这个面板——但代价也是实实在在的：任何能连到这个端口的主机，都能在你屏幕上的置顶窗口里显示任意内容。在家庭网络或 Tailscale 之类的环境下没有问题。如果是在你不完全信任的网络上，请把 `bind` 设为 `"127.0.0.1:7788"`。

Windows 首次运行时会询问是否允许该端口通过防火墙。选择「仅限专用网络」即与上述设定相符。

---

## 行为细节

以下描述原有 Windows 界面的交互；macOS 的差异和验证方法见上文。

**位置。** 面板会在光标附近打开——这是屏幕上唯一能保证你正在看的地方。它会像右键菜单一样在屏幕边缘自动翻转方向，被限制在工作区内（绝不会挡在任务栏下面），并且支持多显示器下的按屏 DPI 感知。它永远不会正好出现在光标热点正下方，因为一个刚好在指针下方冒出来的窗口，会吃掉你原本准备点下去的那次点击。

**拖拽即固定。** 拖拽这个动作本身就是在说「放这里」，所以它也是让面板停止跟随光标的手势。托盘图标 → 双击即可重置回跟随光标模式。不需要专门找一个设置项。

**关闭通知。** 点击一行的任意位置即可关闭它，如果该行绑定了 `--action`，会先执行它。这里没有每行专属的 ✕ 按钮——一个只是缩小版、功能却和整行点击完全一样的小图标，只会让同样的操作变成一个更小的点击目标，而这恰恰是这个面板想要摆脱的交互方式。底部的按钮会一次性清空所有通知并隐藏面板；只清空不隐藏的话，会留下一个空面板杵在那里。

**过期。** 只有当面板可见、该行处于滚动可视区域内、指针没有停留在面板上、并且你在过去 30 秒内使用过键盘或鼠标时，这一行的倒计时才会走。哪怕你在构建过程中离开，回来时结果依然还在。

**只在你不在时弹出。** `--if-idle <seconds>` 会让一条通知先进入列表，但不打开面板，除非键盘和鼠标已经这么长时间没有被触碰。这适用于那些无论你在不在都会触发的事件——比如一轮对话结束、一次构建完成。如果你当时就坐在那里，你已经知道结果了；被打断去被告知你刚刚已经看到的内容，正是训练你去忽略下一次通知的方式。

**全屏游戏与演示模式。** 当 Windows 报告处于免打扰状态时——独占全屏 D3D、演示模式、专注助手忙碌——`low` 和 `normal` 级别的通知会被静默收进列表而不打开面板，等这个状态结束后面板会自动打开。这期间不会丢失任何东西：隐藏状态下的面板不会计算 TTL 倒计时，所以它们只是静静等待，而不会在游戏背后悄悄过期。`critical` 级别依然会强制弹出，因为一条三小时后才看到的警报根本算不上警报。可以通过 `respect_quiet_hours = false` 完全关闭这套逻辑。

**焦点。** 面板使用 `WS_EX_NOACTIVATE`——它可以在你打字的时候出现，而不会吃掉你的一个按键，也不会打断输入法的组字过程。Esc 只在指针停留在面板上时才会被注册为热键，因此它绝不会从你正在使用的应用里抢走 Esc 键。

**空闲。** 90 秒没有任何内容需要显示后，D3D/D2D/DComp 这套渲染栈会被释放，工作集随之收缩。进程本身、管道和 HTTP 监听器都保持运行；下一条通知只需要承担一次性的重建开销，而不是每条通知都要付出启动一个进程的代价。

---

## 架构

通知数据和 HTTP/hook 传输为公共核心，平台依赖按目标系统编译。Windows 保留命名管道和 Win32 消息循环，macOS 使用 Unix domain socket 和 AppKit 主线程：

```
blip CLI ── Windows 命名管道 / macOS Unix socket ─┐
HTTP / Claude Code hook ────────────────────────┤
                                              Command → Store
                                                ├─ Windows: Win32 + D2D/DComp
                                                └─ macOS: NSPanel + NSStatusItem
```

Windows 实现：

```
blip.exe   (控制台程序)   解析参数 → 命名管道 → 退出          约 8ms
                              │ 若守护进程不存在则拉起
blipd.exe  (窗口程序)   ┌──────┴─────────────────────────┐
                       │ 管道线程 ─┐                     │
                       │ HTTP 线程 ─┼→ mpsc → PostMessage
                       │              │                 │
                       │       消息循环 ── 状态存储 ── 布局 ── D2D/DComp
                       └────────────────────────────────┘
```

三种传输方式，一个统一的 `Command` 类型，一套策略引擎。

**为什么不用 Windows 服务：** 服务运行在 Session 0 中，物理上就无法在用户桌面上绘制内容。任何要显示界面的东西都必须是运行在用户会话内的进程。

**为什么交换链是固定大小的：** 它在面板可能达到的最大尺寸下一次性分配完成。内容变化时移动的是*窗口*本身，DComp 会把渲染表面裁剪到窗口大小——如果在动画过程中调整交换链尺寸会导致画面闪烁。

| 文件 | |
|---|---|
| `src/model.rs` | 线路协议 + 运行时通知对象 |
| `src/store.rs` | 列表、同 id 更新、TTL 状态机 |
| `src/config.rs` | TOML 配置，带有可直接使用的默认值 |
| `src/ipc/pipe.rs` | 命名管道；同时也是单实例锁 |
| `src/ipc/unix.rs` | macOS 的本地 socket 传输和单实例控制 |
| `src/ipc/http.rs` | 手写的 HTTP 实现，不依赖异步运行时 |
| `src/ipc/hook.rs` | Claude Code hook 载荷 → 通知对象 |
| `src/ui/layout.rs` | 几何布局逻辑——渲染器和命中测试共用 |
| `src/ui/render.rs` | D3D11 → composition swapchain → D2D → DComp |
| `src/ui/window.rs` | 窗口、消息循环、拖拽、悬停、显示/隐藏 |
| `src/ui/position.rs` | 光标锚定、多显示器 DPI、边缘翻转 |
| `src/macos/` | 原生面板、菜单栏、声音和系统状态 |
| `scripts/package-macos.sh` | 打包 `Blip.app`，包含 CLI 和守护进程 |

不依赖 GPU 就能测试的逻辑都已覆盖：`cargo test` 覆盖了 TTL 暂停规则、id 折叠、淘汰逻辑、边缘翻转、光标避让，以及「命中测试结果与实际绘制内容一致」这一不变量。

---

## 配置

Windows 的配置路径是 `%APPDATA%\blip\config.toml`，macOS 是 `~/Library/Application Support/blip/config.toml`，均由 `blip --config` 创建。每个字段都有可直接使用的默认值；配置文件格式错误时会回退到默认值，并以 critical 通知的形式提示自己出了问题，而不是拒绝启动。下面是 Windows 的示例；macOS 默认 `font = "system"`，使用系统字体。

```toml
bind = "0.0.0.0:7788"     # 无身份验证——参见上文 HTTP 部分的警告
max_items = 50
max_visible_rows = 10     # 面板最多长到这么多行，之后开始滚动
width = 340.0
font = "Microsoft YaHei UI"

[levels]
low_ttl = 4.0
normal_ttl = 7.0
critical_ttl = 0.0     # 0 = 永不过期
low_pops = false       # low 级别只进列表，不会自动弹出面板

[behavior]
cursor_gap = 18.0
drag_to_pin = true
idle_release = 90.0
```

---

## 开机自启

### Windows

可选项——CLI 会按需拉起守护进程。可以在托盘菜单里勾选「开机自动启动」，或者在安装程序里勾选对应的复选框；两者写入的是同一个按用户级别的 `Run` 键值，如果这个值指向了一个失效的路径，守护进程在启动时会自动重写它。

之所以用按用户的 `Run` 而不是服务：Windows 服务运行在 Session 0 中，物理上无法在你的桌面上绘制内容。

如果你想设置一个开机延迟、避免在登录时的磁盘 I/O 高峰凑热闹，可以改用任务计划程序，并把托盘里的开关保持关闭：

```
schtasks /create /tn "blip" /tr "\"C:\path\to\blipd.exe\"" /sc onlogon /delay 0000:30 /f
```

### macOS

CLI 同样会按需启动应用。若要登录时常驻，可以把 `~/Applications/Blip.app` 加入系统的登录项：macOS 13+ 在「系统设置 → 通用 → 登录项」，macOS 11/12 在「系统偏好设置 → 用户与群组 → 登录项」。当前菜单栏没有自动写入登录项的开关。

---

## 安装程序

```powershell
.\build-installer.ps1          # cargo build + test，然后用 ISCC 生成 dist\blip-<ver>-setup.exe
```

需要 Inno Setup 6（`scoop install extras/inno-setup`）。生成的安装程序是按用户安装的，全程不会弹出提权提示：二进制文件会放到 `%LOCALAPPDATA%\Programs\blip`，并带有 PATH 和开机自启的可选复选框。卸载时会（礼貌地）通过 `blip --quit` 停止守护进程，让托盘图标随之消失，同时移除 PATH 条目和 `Run` 键值，`%APPDATA%\blip` 目录默认会保留，除非你另外指定。

### 发布流程

`.github/workflows/ci.yml` 会在 PR 中执行 Linux、Windows 和 macOS 的测试及严格 Clippy 检查，并分别生成 Apple Silicon 和 Intel 的 macOS 开发包。共享核心可在 Linux 测试；AppKit 编译和打包由 macOS runner 完成。以下 tag 发布流程仍只负责 Windows。

推送一个 tag，`.github/workflows/release.yml` 会完成剩下的一切——测试、clippy 检查、生成安装程序、打包便携版 zip、生成 SHA-256 校验和，并创建一个 GitHub Release：

```bash
# Cargo.toml 里的版本号必须已经和 tag 一致；否则工作流会直接失败，
# 因为不然的话，一个 v0.2.0 的 tag 就会发布出 blip-0.1.0-setup.exe。
git tag v0.1.0 && git push origin v0.1.0
```

`workflow_dispatch` 会跑完整套流程并把产物附加到本次运行上，但不会真正发布，适合用来单独测试这条流水线本身。

修改 `installer\blip.iss` 时有两个坑值得注意：

- **花括号会提前结束 Pascal 注释。** `{ ... {app} ... }` 会提前终止注释，导致注释里剩下的部分被当作代码编译。
- **`Pos()` 和 `Copy()`/`Length()` 在处理非 ASCII 字符串时结果不一致。** 在用户名是中日韩字符的机器上，`Pos(';', S)` 返回的是 40，而 `Copy()` 认为分号在第 38 位——差值正好是它前面宽字符的个数。因此手写的按 `;` 分割逻辑，每一段 PATH 都会读错位、匹配不到任何内容，一旦 `Pos` 越界返回 0 甚至可能死循环。出于这个原因，两个 PATH 相关的辅助函数都完全避开了基于下标的字符串运算。

---

## 已知局限

- 列表可以滚动，但没有惯性效果。
- Windows 没有提供按需进入免打扰状态的方式，因此「免打扰」这条路径是通过一个替代信号来测试的：把 `BLIP_QUIET_FILE` 设为某个路径后，守护进程会把「这个文件存在」当作「Windows 说现在要保持安静」。正常使用时不要设置这个变量。曾经有一次 120 秒的测试运行未能正确释放该状态，且之后没能再复现——最可能的原因是文件轮询产生的瞬时误判，真实 API 不会有这种情况，但这一点尚未被证实。
- 没有历史记录面板；清空了就是清空了。这个边界是刻意设定的——一旦它长出已读/未读状态和搜索功能，它就变成了这个项目最初想要摆脱的那种通知中心。
- `--source` 已经端到端贯通，但目前还没有被用于分组或静音。
- 在无人值守（headless）运行时，hook 的 HTTP 请求失败是静默的，二进制程序里对应的失败路径只是一条日志记录，而不是界面上的提示。在交互式 TUI 中的情况尚未验证：blip 被停止时那里是否会打印一行提示，目前不确定。
- 面板无法覆盖独占全屏的 D3D 内容，也无法覆盖 UAC 安全桌面。没有经过签名的 `uiAccess` 二进制程序，任何软件都做不到这一点。
