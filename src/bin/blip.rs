//! The CLI client.
//!
//! Console subsystem so it composes in a pipeline. It does almost nothing:
//! parse args, write one message to the named pipe, exit. Typical round trip is
//! well under 10ms because the expensive parts — GPU device, audio device,
//! window — already exist inside the daemon.
//!
//! Arguments are parsed by hand rather than with a derive-macro crate. This
//! binary is invoked constantly, often several times per script, and every
//! dependency is startup latency paid on each one.

use std::io::Read;

use blip::config::Config;
use blip::ipc::pipe;
use blip::model::{Command, Level, NotifyRequest};

const HELP: &str = r#"blip — cursor-anchored notification panel

USAGE:
    blip [OPTIONS] [TITLE]
    <command> ; blip --exit-code $LASTEXITCODE "任务结束"
    cargo test 2>&1 | blip --stdin -t "测试输出"

OPTIONS:
    -t, --title <TEXT>       Headline. Also the first positional argument.
    -b, --body <TEXT>        Secondary line, wrapped and dimmed.
    -l, --level <LEVEL>      low | normal | critical      [default: normal]
        --id <ID>            Same id replaces that row in place instead of
                             adding one. This is how a long task reports
                             progress without stacking up notifications.
    -s, --source <NAME>      Origin tag, used for grouping and muting.
        --ttl <SECONDS>      Override the level's lifetime. 0 = never expires.
        --sticky             Shorthand for --ttl 0.
        --progress <0-100>   Show a progress bar. Implies an in-flight task, so
                             repeated updates won't keep re-opening the panel.
        --if-idle <SECONDS>  Only open the panel if the keyboard and mouse have
                             been untouched this long; otherwise land in the
                             list quietly. For results you may have watched
                             arrive.
        --action <COMMAND>   Shell command run when the row is clicked.
        --stdin              Read the body from stdin.
        --exit-code <N>      Pick the level from a process exit code:
                             0 -> normal, anything else -> critical.

COMMANDS:
        --dismiss <ID>       Withdraw a notification early.
        --clear              Clear the list and hide the panel.
        --show               Open the panel without adding anything.
        --quit               Shut the daemon down. Never starts one, and exits
                             0 if none was running.
        --config             Print the config path, creating a default if needed.
    -h, --help
    -V, --version

REMOTE:
    curl -d "构建完成" http://127.0.0.1:7788/notify
    curl -X POST http://127.0.0.1:7788/notify -H 'Content-Type: application/json' \
         -d '{"title":"部署失败","level":"critical","id":"deploy"}'
"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match parse(&args) {
        Ok(Some(cmd)) => {
            if let Err(e) = deliver(&cmd) {
                eprintln!("blip: {e}");
                std::process::exit(1);
            }
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("blip: {e}\n\nTry `blip --help`.");
            std::process::exit(2);
        }
    }
}

/// `Ok(None)` means the request was handled locally (help, version, config).
fn parse(args: &[String]) -> Result<Option<Command>, String> {
    if args.is_empty() {
        print!("{HELP}");
        return Ok(None);
    }

    let mut req = NotifyRequest::default();
    let mut positional: Option<String> = None;
    let mut use_stdin = false;
    let mut exit_code: Option<i32> = None;
    let mut i = 0;

    // Consumes the value after a flag, with a clear error rather than silently
    // treating the next flag as the value.
    let need = |i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i).cloned().ok_or_else(|| format!("{flag} needs a value"))
    };

    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("blip {}", blip::VERSION);
                return Ok(None);
            }
            "--config" => {
                let p = Config::path();
                if !p.exists() {
                    Config::write_default().map_err(|e| format!("could not write config: {e}"))?;
                }
                println!("{}", p.display());
                return Ok(None);
            }

            "--clear" => return Ok(Some(Command::Clear)),
            "--quit" => return Ok(Some(Command::Quit)),
            "--show" => return Ok(Some(Command::Show)),
            "--dismiss" => {
                return Ok(Some(Command::Dismiss { id: need(&mut i, "--dismiss")? }));
            }

            "-t" | "--title" => req.title = need(&mut i, "--title")?,
            "-b" | "--body" => req.body = Some(need(&mut i, "--body")?),
            "-s" | "--source" => req.source = Some(need(&mut i, "--source")?),
            "--id" => req.id = Some(need(&mut i, "--id")?),
            "--action" => req.action = Some(need(&mut i, "--action")?),
            "--sound" => req.sound = Some(need(&mut i, "--sound")?),

            "-l" | "--level" => {
                let v = need(&mut i, "--level")?;
                req.level = Some(
                    Level::parse(&v).ok_or_else(|| format!("unknown level `{v}`"))?,
                );
            }
            "--ttl" => {
                let v = need(&mut i, "--ttl")?;
                req.ttl = Some(v.parse().map_err(|_| format!("--ttl `{v}` is not a number"))?);
            }
            "--sticky" => req.ttl = Some(0.0),
            "--if-idle" => {
                let v = need(&mut i, "--if-idle")?;
                req.if_idle =
                    Some(v.parse().map_err(|_| format!("--if-idle `{v}` is not a number"))?);
            }
            "--progress" => {
                let v = need(&mut i, "--progress")?;
                let n: u32 =
                    v.parse().map_err(|_| format!("--progress `{v}` is not a number"))?;
                req.progress = Some(n.min(100) as u8);
            }
            "--exit-code" => {
                let v = need(&mut i, "--exit-code")?;
                exit_code = Some(v.parse().map_err(|_| format!("--exit-code `{v}` is not a number"))?);
            }
            "--stdin" => use_stdin = true,

            _ if a.starts_with('-') && a.len() > 1 => {
                return Err(format!("unknown option `{a}`"));
            }
            _ => {
                if positional.is_none() {
                    positional = Some(a.to_string());
                } else {
                    return Err(format!("unexpected argument `{a}`"));
                }
            }
        }
        i += 1;
    }

    if req.title.is_empty()
        && let Some(p) = positional
    {
        req.title = p;
    }

    if use_stdin {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).map_err(|e| e.to_string())?;
        let text = buf.trim_end();
        if req.title.is_empty() {
            // No title given: first line becomes the headline, rest the body.
            let mut lines = text.splitn(2, '\n');
            req.title = lines.next().unwrap_or("").trim().to_string();
            req.body = lines.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        } else {
            req.body = Some(text.to_string()).filter(|s| !s.is_empty());
        }
    }

    // Exit codes are the most common thing a script has to report, so make the
    // level and a sensible default title fall out of it automatically.
    if let Some(code) = exit_code {
        if req.level.is_none() {
            req.level = Some(if code == 0 { Level::Normal } else { Level::Critical });
        }
        if req.title.is_empty() {
            req.title =
                if code == 0 { "完成".to_string() } else { format!("失败（退出码 {code}）") };
        } else if code != 0 && req.body.is_none() {
            req.body = Some(format!("退出码 {code}"));
        }
    }

    if req.title.trim().is_empty() {
        return Err("nothing to send — give a title, or use --stdin".into());
    }

    Ok(Some(Command::Notify(req)))
}

/// Send, and if nobody is listening, start the daemon and try once more.
///
/// Doing this automatically is what lets the daemon stay an implementation
/// detail: the user only ever types `blip`, and never has to know that
/// something has to be running first.
fn deliver(cmd: &Command) -> Result<(), String> {
    if pipe::send(cmd).is_ok() {
        return Ok(());
    }

    // The one command that must not auto-spawn. "Nothing was running" is the
    // outcome `--quit` asked for, so it is success — an uninstaller calls this
    // unconditionally and should not fail on a machine where blip was already
    // stopped.
    if matches!(cmd, Command::Quit) {
        return Ok(());
    }

    spawn_daemon()?;
    if !pipe::wait_ready(3000) {
        return Err("daemon did not come up within 3s".into());
    }
    pipe::send(cmd)
}

fn spawn_daemon() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // Detached, so the daemon isn't killed when this short-lived CLI process
    // exits and takes its console down with it.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let daemon = exe.with_file_name("blipd.exe");
    if !daemon.exists() {
        return Err(format!("blipd.exe not found next to {}", exe.display()));
    }

    // Detach our standard handles from the child before spawning.
    //
    // `Stdio::null()` alone is not enough, and the reason is a real Win32 trap:
    // `CreateProcess` is called with `bInheritHandles = TRUE`, which hands the
    // child *every* inheritable handle in this process — not just the three it
    // was told about. So the daemon ends up holding a stray copy of whatever
    // pipe our caller gave us, for its entire life.
    //
    // The symptom is nasty: `blip "x" | Out-Null` or `$(blip ...)` blocks
    // forever, because the reader waits for an EOF that only arrives when the
    // daemon exits — and only on the one invocation that happened to be the
    // first, since later ones never spawn anything.
    //
    // Clearing HANDLE_FLAG_INHERIT affects inheritance only; our own writes to
    // stdout below still work normally.
    clear_std_handle_inheritance();

    std::process::Command::new(&daemon)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start blipd: {e}"))?;
    Ok(())
}

fn clear_std_handle_inheritance() {
    use windows::Win32::Foundation::{HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};
    use windows::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if let Ok(h) = unsafe { GetStdHandle(which) }
            && !h.is_invalid()
        {
            let _ = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
        }
    }
}
