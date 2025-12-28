use anyhow::{Context, Result};
use mxr_protocol::{
    decode_msg, encode_msg, ClientToDaemon, DaemonToClient, PaneId, PROTOCOL_VERSION, SessionId,
};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::os::unix::io::RawFd;
// IntoRawFd provided by rustix_openpty::rustix::fd
use std::ffi::CString;
use std::time::Instant;
use tracing::{debug, error, info};

use mio::net::{UnixListener, UnixStream};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use rustix_openpty::rustix::fd::{AsRawFd, OwnedFd, IntoRawFd};
use rustix_openpty::rustix::io::{ioctl_fionbio, Errno};
use nix::unistd::{fork, ForkResult, setsid, dup2, execvp};
use nix::unistd::close;
use libc;
use nix::libc::winsize;
use nix::libc::TIOCSWINSZ;

#[allow(dead_code)]
struct Session {
    id: SessionId,
    name: String,
    created_at: Instant,
    root_pane: Token,
}

#[allow(dead_code)]
struct Pane {
    id: PaneId,
    fd: RawFd,
    master: OwnedFd,
    buf: Vec<u8>,
    child: Option<nix::unistd::Pid>,
}

impl Pane {
    fn new(id: PaneId, pty: rustix_openpty::Pty) -> io::Result<Self> {
        // 非ブロッキング設定
        ioctl_fionbio(&pty.controller, true).map_err(errno_to_io)?;
        let master = pty.controller;
        let fd = master.as_raw_fd();

        let child = spawn_shell(pty.user)?;

        Ok(Self {
            id,
            fd,
            master,
            buf: Vec::with_capacity(4096),
            child: Some(child),
        })
    }

    /// return Ok(Some(n)) if read bytes, Ok(None) on EOF
    fn read_pty(&mut self) -> io::Result<Option<usize>> {
        let mut tmp = [0u8; 4096];
        match rustix_openpty::rustix::io::read(&self.master, &mut tmp) {
            Ok(0) => Ok(None),
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(Some(n))
            }
            Err(err) if err == Errno::AGAIN || err == Errno::WOULDBLOCK => Ok(Some(0)),
            Err(err) => Err(errno_to_io(err)),
        }
    }
}

struct State {
    sessions: HashMap<SessionId, Session>,
    next_session_id: SessionId,
    panes: HashMap<Token, Pane>,
    pane_token_by_id: HashMap<PaneId, Token>,
    next_pane_id: PaneId,
    next_token: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            next_session_id: 0,
            panes: HashMap::new(),
            pane_token_by_id: HashMap::new(),
            next_pane_id: 0,
            next_token: 1,
        }
    }
}

impl State {
    fn alloc_token(&mut self) -> Token {
        let t = self.next_token;
        self.next_token += 1;
        Token(t)
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let socket = socket_path();
    prepare_socket_path(&socket)?;

    let mut listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind socket at {}", socket.display()))?;
    info!("mxrd listening on {}", socket.display());

    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(128);
    let wake = Waker::new(poll.registry(), Token(usize::MAX))?;

    const LISTENER: Token = Token(0);
    poll.registry()
        .register(&mut listener, LISTENER, Interest::READABLE)?;

    let mut state = State::default();
    let mut clients: HashMap<Token, ClientConn> = HashMap::new();

    loop {
        poll.poll(&mut events, None)?;
        for event in events.iter() {
            match event.token() {
                LISTENER => {
                    loop {
                        match listener.accept() {
                            Ok((mut stream, _addr)) => {
                                let token = state.alloc_token();
                                poll.registry().register(
                                    &mut stream,
                                    token,
                                    Interest::READABLE,
                                )?;
                                clients.insert(token, ClientConn::new(stream));
                                debug!("client accepted token={:?}", token);
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) => {
                                error!("accept error: {e:?}");
                                break;
                            }
                        }
                    }
                }
                token if token == Token(usize::MAX) => {
                    // waker — no-op for now
                    let _ = &wake;
                }
                token => {
                    if let Some(conn) = clients.get_mut(&token) {
                        if event.is_readable() {
                            match conn.read_messages(&mut state, poll.registry()) {
                                Ok(ready) => {
                                    if !ready {
                                        // Connection closed
                                        clients.remove(&token);
                                        continue;
                                    }
                                }
                                Err(e) => {
                                    error!("read error {token:?}: {e:?}");
                                    clients.remove(&token);
                                    continue;
                                }
                            }
                        }
                        if event.is_writable() {
                            if let Err(e) = conn.flush_write_buf() {
                                error!("write error {token:?}: {e:?}");
                                clients.remove(&token);
                                continue;
                            }
                        }

                        // Update interest based on pending writes.
                        let interest = if conn.has_pending_write() {
                            Interest::READABLE.add(Interest::WRITABLE)
                        } else {
                            Interest::READABLE
                        };
                        if let Err(e) = poll
                            .registry()
                            .reregister(&mut conn.stream, token, interest)
                        {
                            error!("reregister error {token:?}: {e:?}");
                            clients.remove(&token);
                        }
                    } else if let Some(pane) = state.panes.get_mut(&token) {
                        if event.is_readable() {
                            match pane.read_pty() {
                                Ok(Some(n)) => {
                                    debug!("pane {:?} read {} bytes", token, n);
                                    // fan-out to attached clients
                                    if !pane.buf.is_empty() {
                                        let data = std::mem::take(&mut pane.buf);
                                        for (ctok, client) in clients.iter_mut() {
                                            if client.attached_pane == Some(token) {
                                                client.queue(DaemonToClient::PaneData {
                                                    pane: pane.id,
                                                    data: data.clone(),
                                                })?;
                                                // 確実に書き出すため WRITABLE を有効化
                                                poll.registry().reregister(
                                                    &mut client.stream,
                                                    *ctok,
                                                    Interest::READABLE.add(Interest::WRITABLE),
                                                )?;
                                            }
                                        }
                                    }
                                }
                                Ok(None) => {
                                    debug!("pane {:?} closed", token);
                                    let mut source = SourceFd(&pane.fd);
                                    if let Err(e) = poll.registry().deregister(&mut source) {
                                        error!("deregister pane {:?}: {e:?}", token);
                                    }
                                    state.panes.remove(&token);
                                }
                                Err(e) => {
                                    error!("pane read error {:?}: {e:?}", token);
                                    let mut source = SourceFd(&pane.fd);
                                    if let Err(e2) = poll.registry().deregister(&mut source) {
                                        error!("deregister pane {:?}: {e2:?}", token);
                                    }
                                    state.panes.remove(&token);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn socket_path() -> PathBuf {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.as_os_str().len() > 0)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    runtime_dir.join("mxr.sock")
}

fn prepare_socket_path(socket: &Path) -> io::Result<()> {
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }
    if socket.exists() {
        fs::remove_file(socket)?;
    }
    Ok(())
}

fn register_new_pane(state: &mut State, registry: &mio::Registry) -> io::Result<Token> {
    let token = state.alloc_token();
    let pty = rustix_openpty::openpty(None, None)
        .map_err(|e| io::Error::from_raw_os_error(e.raw_os_error()))?;
    let pane = Pane::new(state.next_pane_id, pty)?;
    state.next_pane_id += 1;
    {
        let mut source = SourceFd(&pane.fd);
        registry.register(&mut source, token, Interest::READABLE)?;
    }
    state.pane_token_by_id.insert(pane.id, token);
    state.panes.insert(token, pane);
    Ok(token)
}

fn errno_to_io(err: Errno) -> io::Error {
    io::Error::from_raw_os_error(err.raw_os_error())
}

/// フォークしてユーザ側 PTY にシェルを exec する。
fn spawn_shell(user_fd: OwnedFd) -> io::Result<nix::unistd::Pid> {
    let raw = user_fd.into_raw_fd();
    match unsafe { fork().map_err(|e| io::Error::from_raw_os_error(e as i32))? } {
        ForkResult::Child => {
            // 新しいセッションで制御端末にする
            setsid().map_err(|e| io::Error::from_raw_os_error(e as i32))?;
            unsafe {
                // ignore failure
                libc::ioctl(raw, libc::TIOCSCTTY.into(), 0);
            }
            // 標準入出力を接続
            dup2(raw, 0).map_err(|e| io::Error::from_raw_os_error(e as i32))?;
            dup2(raw, 1).map_err(|e| io::Error::from_raw_os_error(e as i32))?;
            dup2(raw, 2).map_err(|e| io::Error::from_raw_os_error(e as i32))?;
            if raw > 2 {
                let _ = close(raw);
            }
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let c = CString::new(shell.clone()).unwrap();
            let args = [c.clone()];
            let err = execvp(&c, &args).unwrap_err();
            eprintln!("exec {} failed: {err}", shell);
            std::process::exit(127);
        }
        ForkResult::Parent { child } => {
            // 親では user 側 FD を閉じる
            let _ = close(raw);
            Ok(child)
        }
    }
}

// ----- connection handling -----

struct ClientConn {
    stream: UnixStream,
    len_buf: [u8; 4],
    len_filled: usize,
    expected_payload: Option<usize>,
    payload: Vec<u8>,
    handshaken: bool,
    write_buf: Vec<u8>,
    attached_pane: Option<Token>,
}

impl ClientConn {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            len_buf: [0; 4],
            len_filled: 0,
            expected_payload: None,
            payload: Vec::new(),
            handshaken: false,
            write_buf: Vec::new(),
            attached_pane: None,
        }
    }

    /// Returns Ok(false) when peer closed.
    fn read_messages(&mut self, state: &mut State, registry: &mio::Registry) -> Result<bool> {
        loop {
            // read length prefix
            if self.expected_payload.is_none() {
                match self.stream.read(&mut self.len_buf[self.len_filled..]) {
                    Ok(0) => return Ok(false),
                    Ok(n) => {
                        self.len_filled += n;
                        if self.len_filled < 4 {
                            return Ok(true);
                        }
                        let len = u32::from_be_bytes(self.len_buf) as usize;
                        self.expected_payload = Some(len);
                        self.payload.clear();
                        self.payload.reserve(len);
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                    Err(e) => return Err(e.into()),
                }
            }

            // read payload
            if let Some(len) = self.expected_payload {
                let to_read = len - self.payload.len();
                let mut buf = vec![0u8; to_read];
                match self.stream.read(&mut buf) {
                    Ok(0) => return Ok(false),
                    Ok(n) => {
                        self.payload.extend_from_slice(&buf[..n]);
                        if self.payload.len() < len {
                            return Ok(true);
                        }
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                    Err(e) => return Err(e.into()),
                }

                if self.payload.len() == len {
                    self.process_one(state, registry)?;
                    // reset to read next frame
                    self.len_buf = [0; 4];
                    self.len_filled = 0;
                    self.expected_payload = None;
                    self.payload.clear();
                }
            }
        }
    }

    fn process_one(&mut self, state: &mut State, registry: &mio::Registry) -> Result<()> {
        let msg: ClientToDaemon = decode_msg(&self.payload)?;
        if !self.handshaken {
            if let ClientToDaemon::Hello { version, capabilities: _ } = msg {
                if version != PROTOCOL_VERSION {
                    self.queue(DaemonToClient::Error {
                        message: format!(
                            "protocol mismatch: daemon {PROTOCOL_VERSION}, client {version}"
                        ),
                    })?;
                } else {
                    self.handshaken = true;
                    self.queue(DaemonToClient::HelloAck { version })?;
                }
            } else {
                self.queue(DaemonToClient::Error {
                    message: "send Hello first".into(),
                })?;
            }
            return Ok(());
        }

        match msg {
            ClientToDaemon::CreateSession { name } => {
                let session_id = state.next_session_id;
                state.next_session_id += 1;
                let pane_token = register_new_pane(state, registry)?;
                let pane_id = state.panes.get(&pane_token).map(|p| p.id).unwrap_or(0);
                let session = Session {
                    id: session_id,
                    name: name.clone(),
                    created_at: Instant::now(),
                    root_pane: pane_token,
                };
                state.sessions.insert(session_id, session);
                state.pane_token_by_id.insert(pane_id, pane_token);
                debug!("session {} created with pane {:?}", session_id, pane_token);
                self.queue(DaemonToClient::SessionCreated { session: session_id })?;
                self.queue(DaemonToClient::AttachOk { pane: pane_id })?;
                self.attached_pane = Some(pane_token);
            }
            ClientToDaemon::Ping(nonce) => {
                self.queue(DaemonToClient::Pong(nonce))?;
            }
            ClientToDaemon::Attach { session } => {
                if let Some(sess) = state.sessions.get(&session) {
                    self.attached_pane = Some(sess.root_pane);
                    let pane_id = state.panes.get(&sess.root_pane).map(|p| p.id).unwrap_or(0);
                    self.queue(DaemonToClient::AttachOk { pane: pane_id })?;
                } else {
                    self.queue(DaemonToClient::Error {
                        message: format!("no such session {session}"),
                    })?;
                }
            }
            ClientToDaemon::Stdin { pane, data } => {
                if let Some(token) = state.pane_token_by_id.get(&pane).copied() {
                    if let Some(p) = state.panes.get_mut(&token) {
                        let mut written = 0;
                        while written < data.len() {
                            match rustix_openpty::rustix::io::write(&p.master, &data[written..]) {
                                Ok(0) => break,
                                Ok(n) => written += n,
                                Err(e) if e == Errno::WOULDBLOCK || e == Errno::AGAIN => break,
                                Err(e) => {
                                    self.queue(DaemonToClient::Error {
                                        message: format!("write failed: {e:?}"),
                                    })?;
                                    break;
                                }
                            }
                        }
                    } else {
                        self.queue(DaemonToClient::Error {
                            message: "pane token missing".into(),
                        })?;
                    }
                } else {
                    self.queue(DaemonToClient::Error {
                        message: format!("no such pane {pane}"),
                    })?;
                }
            }
            ClientToDaemon::Resize { pane, cols, rows } => {
                if let Some(token) = state.pane_token_by_id.get(&pane).copied() {
                    if let Some(p) = state.panes.get_mut(&token) {
                        let ws = winsize {
                            ws_row: rows,
                            ws_col: cols,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        };
                        unsafe {
                            libc::ioctl(p.master.as_raw_fd(), TIOCSWINSZ.into(), &ws);
                        }
                    }
                }
            }
            ClientToDaemon::Detach => {
                self.attached_pane = None;
                self.queue(DaemonToClient::Pong(0))?;
            }
            other => {
                self.queue(DaemonToClient::Error {
                    message: format!("unsupported request: {other:?}"),
                })?;
            }
        }
        Ok(())
    }

    fn queue(&mut self, msg: DaemonToClient) -> Result<()> {
        let encoded = encode_msg(&msg)?;
        self.write_buf
            .extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        self.write_buf.extend_from_slice(&encoded);
        Ok(())
    }

    fn flush_write_buf(&mut self) -> io::Result<()> {
        while !self.write_buf.is_empty() {
            match self.stream.write(&self.write_buf) {
                Ok(0) => {
                    // Socket not ready; try again when writable.
                    break;
                }
                Ok(n) => {
                    self.write_buf.drain(..n);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Nonfatal for nonblocking sockets; keep data queued.
                    break;
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn has_pending_write(&self) -> bool {
        !self.write_buf.is_empty()
    }
}
