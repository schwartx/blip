//! Per-user local transport for macOS and Unix development hosts.
//!
//! A private directory restricts socket access to the current user. An advisory
//! lock, held for the server's lifetime, serializes startup and stale socket
//! cleanup without deleting another live daemon's endpoint.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::ipc::Bridge;
use crate::model::{Command, NotifyRequest};

const MAX_PAYLOAD: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(1);
const ACCEPT_POLL: Duration = Duration::from_millis(10);

fn uid() -> u32 {
    // No privilege-changing operations take place in either binary.
    unsafe { libc::geteuid() }
}

fn runtime_dir() -> PathBuf {
    // Unix socket paths are short on macOS (104 bytes). The system temporary
    // directory is often much longer, so use this fixed, private uid directory.
    PathBuf::from(format!("/tmp/blip-{}-v1", uid()))
}

pub fn pipe_name() -> String {
    runtime_dir()
        .join("daemon.sock")
        .to_string_lossy()
        .into_owned()
}

fn validate_directory(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("local IPC directory: {e}"))?;
    if !meta.is_dir() || meta.uid() != uid() || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{} must be a private directory owned by the current user",
            path.display()
        ));
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("could not create local IPC directory: {e}")),
    }
    validate_directory(path)
}

pub struct PipeServer {
    listener: UnixListener,
    // Keep the descriptor (and flock) alive until after endpoint cleanup.
    _lock: File,
    path: PathBuf,
    identity: (u64, u64),
    stop: AtomicBool,
}

impl PipeServer {
    pub fn bind() -> Result<Self, String> {
        Self::bind_at(&runtime_dir())
    }

    fn bind_at(dir: &Path) -> Result<Self, String> {
        private_directory(dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir.join("daemon.lock"))
            .map_err(|e| format!("could not open daemon lock: {e}"))?;
        let meta = lock.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.uid() != uid() || meta.mode() & 0o077 != 0 {
            return Err(
                "daemon lock must be a private regular file owned by the current user".into(),
            );
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!(
                "could not claim daemon lock: {}",
                io::Error::last_os_error()
            ));
        }

        let path = dir.join("daemon.sock");
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_socket() && meta.uid() == uid() => {
                // The lock establishes that this endpoint has no live owner.
                fs::remove_file(&path).map_err(|e| format!("could not clean stale socket: {e}"))?;
            }
            Ok(_) => return Err("refusing to replace a non-socket local IPC endpoint".into()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }

        let listener =
            UnixListener::bind(&path).map_err(|e| format!("could not bind local IPC: {e}"))?;
        // The parent directory already makes this private during creation.
        if let Err(e) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&path);
            return Err(e.to_string());
        }
        if let Err(e) = listener.set_nonblocking(true) {
            let _ = fs::remove_file(&path);
            return Err(e.to_string());
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        Ok(Self {
            listener,
            _lock: lock,
            path,
            identity: (meta.dev(), meta.ino()),
            stop: AtomicBool::new(false),
        })
    }

    /// Serial arrival order, with bounded clients so shutdown cannot hang.
    pub fn serve(&self, bridge: Bridge) {
        while !self.stop.load(Ordering::Acquire) {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    if let Ok(bytes) = read_payload(&mut stream) {
                        dispatch(&bytes, &bridge);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        // Do not unlink an endpoint that was replaced after this server bound.
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.file_type().is_socket()
            && (meta.dev(), meta.ino()) == self.identity
        {
            let _ = fs::remove_file(&self.path);
        }
        // Leave the lock file in place: unlinking an acquired lock permits a
        // racing process to create a second inode and claim a second lock.
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "local IPC timed out"))
}

fn wait_io(stream: &UnixStream, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let left = remaining(deadline)?;
        let mut poll = libc::pollfd {
            fd: stream.as_raw_fd(),
            events,
            revents: 0,
        };
        // Round up so a sub-millisecond remainder cannot become a busy poll.
        let milliseconds = left.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
        match unsafe { libc::poll(&mut poll, 1, milliseconds) } {
            -1 => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            0 => {} // Recheck the absolute deadline after a timed wait.
            _ if poll.revents & libc::POLLNVAL != 0 => {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            _ if poll.revents & (events | libc::POLLERR | libc::POLLHUP) != 0 => {
                // Let read/write report EOF or the socket error. POLLHUP may
                // accompany unread data, which must be drained before EOF.
                return Ok(());
            }
            _ => {}
        }
    }
}

fn read_payload(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    // Use the same readiness/deadline rules on Darwin and Linux rather than
    // relying on platform-specific socket timeout behavior after half-close.
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        remaining(deadline)?;
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(n) => {
                if bytes.len() + n > MAX_PAYLOAD {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "local IPC payload exceeds 64 KiB",
                    ));
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait_io(stream, libc::POLLIN, deadline)?;
            }
            Err(e) => return Err(e),
        }
    }
}

fn dispatch(bytes: &[u8], bridge: &Bridge) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let cmd = serde_json::from_str::<Command>(line).unwrap_or_else(|_| {
            Command::Notify(NotifyRequest {
                title: line.into(),
                ..Default::default()
            })
        });
        bridge.send(cmd);
    }
}

fn connect(dir: &Path) -> Result<UnixStream, String> {
    validate_directory(dir)?;
    let path = dir.join("daemon.sock");
    let meta = fs::symlink_metadata(&path).map_err(|e| format!("local IPC not available: {e}"))?;
    if !meta.file_type().is_socket() || meta.uid() != uid() || meta.mode() & 0o077 != 0 {
        return Err("local IPC endpoint must be a private socket owned by the current user".into());
    }
    connect_bounded(&path, IO_TIMEOUT).map_err(|e| format!("local IPC not available: {e}"))
}

fn connect_bounded(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    // UnixStream::connect can block when the listener's pending queue fills.
    // Establish the connection nonblocking, then restore blocking IO with the
    // read/write deadlines below. This also bounds concurrent startup probes.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Unix socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
    }
    for (dest, source) in address.sun_path.iter_mut().zip(bytes) {
        *dest = *source as libc::c_char;
    }
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw == -1 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    loop {
        let result = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        };
        if result == 0 {
            stream.set_nonblocking(false)?;
            return Ok(stream);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => {
                stream.set_nonblocking(false)?;
                return Ok(stream);
            }
            Some(libc::EINPROGRESS) => loop {
                let left = remaining(deadline)?;
                let mut poll = libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let milliseconds = left.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
                match unsafe { libc::poll(&mut poll, 1, milliseconds) } {
                    0 => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "local IPC connect timed out",
                        ));
                    }
                    -1 => {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(error);
                    }
                    _ => {
                        if let Some(error) = stream.take_error()? {
                            return Err(error);
                        }
                        stream.set_nonblocking(false)?;
                        return Ok(stream);
                    }
                }
            },
            _ if error.kind() == io::ErrorKind::WouldBlock => {
                // Linux reports EAGAIN for a full Unix accept queue: there is
                // no connection in flight, so retry after a bounded pause.
                std::thread::sleep(ACCEPT_POLL.min(remaining(deadline)?));
            }
            _ if error.kind() == io::ErrorKind::Interrupted => {
                remaining(deadline)?;
            }
            _ => return Err(error),
        }
    }
}

pub fn send(cmd: &Command) -> Result<(), String> {
    send_at(&runtime_dir(), cmd)
}

fn send_at(dir: &Path, cmd: &Command) -> Result<(), String> {
    let payload = serde_json::to_vec(cmd).map_err(|e| e.to_string())?;
    if payload.len() > MAX_PAYLOAD {
        return Err("local IPC payload exceeds 64 KiB".into());
    }
    let mut stream = connect(dir)?;
    stream.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut written = 0;
    while written < payload.len() {
        remaining(deadline).map_err(|e| e.to_string())?;
        match stream.write(&payload[written..]) {
            Ok(0) => return Err("local IPC write returned zero bytes".into()),
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait_io(&stream, libc::POLLOUT, deadline).map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(format!("local IPC write failed: {e}")),
        }
    }
    stream.shutdown(Shutdown::Write).map_err(|e| e.to_string())
}

pub fn wait_ready(timeout_ms: u32) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.into());
    loop {
        if connect(&runtime_dir()).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(ACCEPT_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};

    static NEXT_DIR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = PathBuf::from(format!(
                "/tmp/blip-test-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            private_directory(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lock_and_cleanup_allow_only_one_live_server() {
        let dir = TestDir::new();
        let first = PipeServer::bind_at(&dir.0).unwrap();
        assert!(PipeServer::bind_at(&dir.0).is_err());
        assert!(dir.0.join("daemon.sock").exists());
        drop(first);
        assert!(!dir.0.join("daemon.sock").exists());
        assert!(PipeServer::bind_at(&dir.0).is_ok());
    }

    #[test]
    fn stale_socket_is_recovered_but_regular_files_are_preserved() {
        let dir = TestDir::new();
        let path = dir.0.join("daemon.sock");
        drop(UnixListener::bind(&path).unwrap());
        drop(PipeServer::bind_at(&dir.0).unwrap());
        fs::write(&path, "preserve this").unwrap();
        assert!(PipeServer::bind_at(&dir.0).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "preserve this");
    }

    #[test]
    fn insecure_directories_and_symlink_locks_are_rejected() {
        let dir = TestDir::new();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(PipeServer::bind_at(&dir.0).is_err());
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(dir.0.join("target"), "untouched").unwrap();
        std::os::unix::fs::symlink("target", dir.0.join("daemon.lock")).unwrap();
        assert!(PipeServer::bind_at(&dir.0).is_err());
        assert_eq!(
            fs::read_to_string(dir.0.join("target")).unwrap(),
            "untouched"
        );
        std::os::unix::fs::symlink(&dir.0, dir.0.join("linked-dir")).unwrap();
        assert!(PipeServer::bind_at(&dir.0.join("linked-dir")).is_err());
    }

    #[test]
    fn commands_arrive_in_order_and_stopping_releases_endpoint() {
        let dir = TestDir::new();
        let server = Arc::new(PipeServer::bind_at(&dir.0).unwrap());
        let (tx, rx) = mpsc::channel();
        let (bridge, _) = Bridge::new(tx);
        let serving = server.clone();
        let thread = std::thread::spawn(move || serving.serve(bridge));
        send_at(
            &dir.0,
            &Command::Notify(NotifyRequest {
                title: "你好 macOS".into(),
                ..Default::default()
            }),
        )
        .unwrap();
        send_at(&dir.0, &Command::Dismiss { id: "build".into() }).unwrap();
        assert!(
            matches!(rx.recv_timeout(IO_TIMEOUT).unwrap(), Command::Notify(req) if req.title == "你好 macOS")
        );
        assert!(
            matches!(rx.recv_timeout(IO_TIMEOUT).unwrap(), Command::Dismiss { id } if id == "build")
        );
        server.stop();
        thread.join().unwrap();
        drop(server);
        assert!(!dir.0.join("daemon.sock").exists());
    }

    #[test]
    fn framing_accepts_multiple_commands_and_rejects_oversized_payloads() {
        let (tx, rx) = mpsc::channel();
        let (bridge, _) = Bridge::new(tx);
        dispatch(b"{\"cmd\":\"show\"}\nhello\n{\"cmd\":\"clear\"}\n", &bridge);
        assert!(matches!(rx.try_recv().unwrap(), Command::Show));
        assert!(matches!(rx.try_recv().unwrap(), Command::Notify(req) if req.title == "hello"));
        assert!(matches!(rx.try_recv().unwrap(), Command::Clear));
        dispatch(&[0xff], &bridge);
        assert!(rx.try_recv().is_err());

        let dir = TestDir::new();
        let _server = PipeServer::bind_at(&dir.0).unwrap();
        let oversized = Command::Notify(NotifyRequest {
            title: "a".repeat(MAX_PAYLOAD),
            ..Default::default()
        });
        assert!(send_at(&dir.0, &oversized).unwrap_err().contains("64 KiB"));
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(move || {
            let _ = writer.write_all(&vec![b'a'; MAX_PAYLOAD + 1]);
        });
        assert_eq!(
            read_payload(&mut reader).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        thread.join().unwrap();
    }

    #[test]
    fn stalled_client_is_bounded() {
        let (_writer, mut reader) = UnixStream::pair().unwrap();
        // Both newly created blocking sockets and inherited nonblocking ones
        // use the same readiness loop and total deadline.
        reader.set_nonblocking(true).unwrap();
        let started = Instant::now();
        let error = read_payload(&mut reader).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        assert!(
            started.elapsed() >= IO_TIMEOUT,
            "a stalled socket must not fail before its deadline"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn delayed_chunks_are_drained_before_half_closed_eof() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(move || {
            writer.write_all(b"{\"cmd\":").unwrap();
            std::thread::sleep(Duration::from_millis(20));
            writer.write_all(b"\"show\"}\n").unwrap();
            writer.shutdown(Shutdown::Write).unwrap();
        });
        assert_eq!(read_payload(&mut reader).unwrap(), b"{\"cmd\":\"show\"}\n");
        thread.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_full_accept_queue_does_not_block_connection_indefinitely() {
        let dir = TestDir::new();
        let server = PipeServer::bind_at(&dir.0).unwrap();
        assert_eq!(unsafe { libc::listen(server.listener.as_raw_fd(), 1) }, 0);
        let timeout = Duration::from_millis(50);
        let first = connect_bounded(&server.path, timeout).unwrap();
        let second = connect_bounded(&server.path, timeout).unwrap();
        let started = Instant::now();
        assert_eq!(
            connect_bounded(&server.path, timeout).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop((first, second));
    }

    #[test]
    fn dropping_a_server_preserves_a_replaced_socket() {
        let dir = TestDir::new();
        let server = PipeServer::bind_at(&dir.0).unwrap();
        fs::remove_file(&server.path).unwrap();
        let replacement = UnixListener::bind(&server.path).unwrap();
        drop(server);
        assert!(dir.0.join("daemon.sock").exists());
        drop(replacement);
    }
}
