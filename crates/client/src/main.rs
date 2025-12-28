use anyhow::{Context, Result};
use mxr_protocol::{
    decode_msg, encode_msg, frame, Capability, ClientToDaemon, DaemonToClient, PROTOCOL_VERSION,
};
use nix::libc::winsize;
use nix::sys::signal::Signal;
use nix::sys::termios::{self, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, Termios};
use signal_hook::flag as signal_flag;
use std::env;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("ping");

    let mut stream = UnixStream::connect(socket_path()).context("connect to daemon failed")?;
    handshake(&mut stream)?;

    match cmd {
        "create" => {
            let name = args.get(2).map(String::as_str).unwrap_or("default");
            let msg = ClientToDaemon::CreateSession {
                name: name.to_string(),
            };
            send(&mut stream, &msg)?;
            if let Some(reply) = recv(&mut stream)? {
                println!("{reply:?}");
            }
        }
        "attach" => {
            let session: u64 = args
                .get(2)
                .expect("session id required")
                .parse()
                .expect("session id must be number");
            send(&mut stream, &ClientToDaemon::Attach { session })?;
            let pane_id = match recv(&mut stream)?.context("missing attach reply")? {
                DaemonToClient::AttachOk { pane } => pane,
                other => anyhow::bail!("attach failed: {other:?}"),
            };
            println!("attached to session {session}, pane {pane_id}");

            // 端末を raw にし、初期サイズ送信
            let mut raw_guard = Some(enable_raw_mode()?);
            send_resize(&mut stream, pane_id)?;
            // stdin を非ブロッキングにし、デーモン切断時にループを抜けやすくする
            let stdin_fd = io::stdin().as_raw_fd();
            let orig_flags = nix::fcntl::fcntl(stdin_fd, nix::fcntl::F_GETFL)?;
            nix::fcntl::fcntl(
                stdin_fd,
                nix::fcntl::F_SETFL(
                    nix::fcntl::OFlag::from_bits_truncate(orig_flags)
                        | nix::fcntl::OFlag::O_NONBLOCK,
                ),
            )?;

            // SIGWINCH 監視
            let winch = Arc::new(AtomicBool::new(false));
            signal_flag::register(Signal::SIGWINCH as i32, Arc::clone(&winch))?;

            let stream_arc = Arc::new(Mutex::new(stream));
            let alive = Arc::new(AtomicBool::new(true));
            let idle_limit = std::env::var("MXR_BENCH_IDLE_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok());
            {
                let s = Arc::clone(&stream_arc);
                let alive = Arc::clone(&alive);
                thread::spawn(move || -> Result<()> {
                    let mut local = s.lock().unwrap().try_clone().context("clone stream")?;
                    loop {
                        match recv(&mut local)? {
                            Some(DaemonToClient::PaneData { data, .. }) => {
                                io::stdout().write_all(&data)?;
                                io::stdout().flush()?;
                            }
                            Some(_) => {}
                            None => {
                                alive.store(false, Ordering::SeqCst);
                                break;
                            }
                        }
                    }
                    Ok(())
                });
            }
            // stdin -> daemon
            let mut stdin = io::stdin();
            let mut buf = [0u8; 1024];
            let mut idle_ms = 0u64;
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        idle_ms = 0;
                        if winch.swap(false, Ordering::SeqCst) {
                            send_resize(&mut *stream_arc.lock().unwrap(), pane_id)?;
                        }
                        let msg = ClientToDaemon::Stdin {
                            pane: pane_id,
                            data: buf[..n].to_vec(),
                        };
                        let mut locked = stream_arc.lock().unwrap();
                        send(&mut *locked, &msg)?;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if let Some(limit) = idle_limit {
                            idle_ms += 2;
                            if idle_ms >= limit {
                                break;
                            }
                        }
                        if !alive.load(Ordering::SeqCst) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }

            if let Some(g) = raw_guard.take() {
                g.restore()?;
            }
        }
        "ping" | _ => {
            send(&mut stream, &ClientToDaemon::Ping(42))?;
            if let Some(reply) = recv(&mut stream)? {
                println!("{reply:?}");
            }
        }
    }
    Ok(())
}

fn handshake(stream: &mut UnixStream) -> Result<()> {
    let hello = ClientToDaemon::Hello {
        version: PROTOCOL_VERSION,
        capabilities: vec![
            Capability::SplitVertical,
            Capability::SplitHorizontal,
            Capability::Tabs,
        ],
    };
    send(stream, &hello)?;
    let reply = recv(stream)?.context("missing handshake reply")?;
    match reply {
        DaemonToClient::HelloAck { version } if version == PROTOCOL_VERSION => Ok(()),
        other => anyhow::bail!("unexpected hello reply: {other:?}"),
    }
}

fn send(stream: &mut UnixStream, msg: &ClientToDaemon) -> Result<()> {
    let encoded = encode_msg(msg)?;
    frame::write(stream, &encoded)?;
    Ok(())
}

fn recv(stream: &mut UnixStream) -> Result<Option<DaemonToClient>> {
    match frame::read(stream) {
        Ok(buf) => Ok(Some(decode_msg(&buf)?)),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn enable_raw_mode() -> Result<RawModeGuard> {
    let fd_raw = nix::libc::STDIN_FILENO;
    let fd = unsafe { BorrowedFd::borrow_raw(fd_raw) };
    let orig = termios::tcgetattr(fd).map_err(|e| anyhow::anyhow!(e))?;
    let mut raw = orig.clone();
    raw.input_flags &= !(InputFlags::BRKINT
        | InputFlags::ICRNL
        | InputFlags::INPCK
        | InputFlags::ISTRIP
        | InputFlags::IXON);
    raw.control_flags |= ControlFlags::CS8;
    raw.local_flags &=
        !(LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::IEXTEN | LocalFlags::ISIG);
    raw.output_flags &= !(OutputFlags::OPOST);
    termios::tcsetattr(fd, SetArg::TCSANOW, &raw).map_err(|e| anyhow::anyhow!(e))?;
    Ok(RawModeGuard { orig, fd_raw })
}

struct RawModeGuard {
    orig: Termios,
    fd_raw: i32,
}

impl RawModeGuard {
    fn restore(self) -> Result<()> {
        let fd = unsafe { BorrowedFd::borrow_raw(self.fd_raw) };
        termios::tcsetattr(fd, SetArg::TCSANOW, &self.orig).map_err(|e| anyhow::anyhow!(e))
    }
}

fn send_resize(stream: &mut UnixStream, pane: mxr_protocol::PaneId) -> Result<()> {
    let ws = winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut ws = ws;
    unsafe {
        if nix::libc::ioctl(nix::libc::STDIN_FILENO, nix::libc::TIOCGWINSZ, &mut ws) != 0 {
            return Ok(()); // 取得失敗時は無視
        }
    }
    let msg = ClientToDaemon::Resize {
        pane,
        cols: ws.ws_col as u16,
        rows: ws.ws_row as u16,
    };
    send(stream, &msg)
}
fn socket_path() -> PathBuf {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.as_os_str().len() > 0)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    runtime_dir.join("mxr.sock")
}
