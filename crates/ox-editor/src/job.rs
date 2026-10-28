//! Reactor-driven child process channels used by Vimscript job control.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
#[cfg(not(unix))]
use std::process::ChildStdin;
#[cfg(not(unix))]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ox_rpc::{IncrementalDecoder, Message, MsgidCounter};
use ox_types::{ApiError, DictRef, Object, OxStr, Typval};
#[cfg(unix)]
use ox_uv::NetEvent;
use ox_uv::{Timer, UvLoop};
use ox_uv::process::{self, Process, PtySize, SpawnOptions, StdioConfig};
#[cfg(unix)]
use ox_uv::process::{ProcessPipe, PtyHandle};

/// Callback values and their dictionary receiver from `jobstart()` options.
#[derive(Clone)]
pub struct JobCallbacks {
    /// Original options dictionary, bound as callback `self`.
    pub options: DictRef,
    /// Standard-output callback.
    pub stdout: Option<Typval>,
    /// Standard-error callback.
    pub stderr: Option<Typval>,
    /// Process-exit callback.
    pub exit: Option<Typval>,
}

/// `pty_proc_init` geometry (os/pty_proc_unix.c:460-461): the size each pty
/// dimension keeps when `jobstart` supplies none (channel.c:394-398).
pub const DEFAULT_PTY_SIZE: PtySize = PtySize {
    columns: 80,
    rows: 24,
};

/// Normalized options for one child process.
#[expect(
    clippy::struct_excessive_bools,
    reason = "these independent Vim job options do not form one state machine"
)]
pub struct JobStartOptions {
    /// Executable path.
    pub program: PathBuf,
    /// Arguments after the executable.
    pub args: Vec<OsString>,
    /// Replacement environment, or inherited environment when absent.
    pub environment: Option<Vec<(OsString, OsString)>>,
    /// Child working directory.
    pub cwd: Option<PathBuf>,
    /// Whether the child outlives editor shutdown.
    pub detached: bool,
    /// Whether stdio uses a pseudoterminal.
    pub pty: bool,
    /// Whether the pseudoterminal backs a `:terminal` buffer.
    pub term: bool,
    /// Geometry a `pty` spawn opens with: the current window's text area
    /// when the caller supplied one, else [`DEFAULT_PTY_SIZE`].
    pub pty_size: PtySize,
    /// Whether the channel carries msgpack-rpc.
    pub rpc: bool,
    /// Whether stdin is connected to a writable pipe.
    pub stdin_pipe: bool,
    /// Whether stdout is delivered once at EOF.
    pub stdout_buffered: bool,
    /// Whether stderr is delivered once at EOF.
    pub stderr_buffered: bool,
    /// Editor buffer that receives PTY output as live text, when allocated.
    pub terminal_buffer: Option<ox_types::BufHandle>,
    /// Deferred callbacks and their receiver.
    pub callbacks: JobCallbacks,
}

/// A callback invocation deferred until after the reactor callback returns.
#[derive(Clone)]
pub struct JobEvent {
    /// Function, partial, or Lua reference to invoke.
    pub callback: Typval,
    /// Options dictionary bound as Vimscript `self`.
    pub receiver: DictRef,
    /// Upstream-compatible callback arguments.
    pub args: Vec<Typval>,
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

enum RawEvent {
    Data(u64, StreamKind, Vec<u8>),
    Eof(u64, StreamKind),
    Exit(u64, Result<process::ProcessExit, String>),
}

#[derive(Default)]
struct StreamState {
    buffered: bool,
    bytes: Vec<u8>,
    eof: bool,
}

/// Cap on bytes held for an unconsumed RPC reply (8 MiB): a peer that floods
/// a channel nobody reads must not exhaust editor memory. Crossing it closes
/// the RPC half instead of growing further.
const MAX_RPC_BUFFER: usize = 8 << 20;

/// Cap on replies stashed while waiting for a different `msgid`: a peer that
/// answers anything but the awaited call must not grow the stash without
/// bound. Crossing it fails the wait and drops the stash.
const MAX_RPC_STASH: usize = 1024;

/// One closable part of a channel (`ChannelPart`, `channel_defs.h`), as
/// `chanclose(id[, stream])` names it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChannelPart {
    /// `"stdin"`.
    Stdin,
    /// `"stdout"`.
    Stdout,
    /// `"stderr"`.
    Stderr,
    /// `"rpc"`.
    Rpc,
    /// Omitted stream: every part.
    All,
}

/// Failure of [`JobManager::close_job_stream`], mirroring the `const char
/// *error` upstream `channel_close` reports: `e_invchan` for an unknown
/// channel, `e_invstream` for a stream the channel does not have, and
/// `e_invstreamrpc` for stdin/stdout of an RPC channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChannelCloseError {
    /// `E900: Invalid channel id`.
    #[error("E900: Invalid channel id")]
    UnknownChannel,
    /// `E906: invalid stream for channel`.
    #[error("E906: invalid stream for channel")]
    InvalidStream,
    /// `E906: invalid stream for rpc channel, use 'rpc'`.
    #[error("E906: invalid stream for rpc channel, use 'rpc'")]
    InvalidRpcStream,
}

/// Failure of [`JobManager::rpc_call`], typed so callers map variants to
/// Vimscript errors without parsing message text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RpcCallError {
    /// The channel does not exist or does not carry msgpack-rpc.
    #[error("E900: Invalid channel id")]
    NotRpc,
    /// The request frame could not be encoded.
    #[error("E475: Invalid argument: {0}")]
    Encode(String),
    /// The frame could not be written to the transport.
    #[error("E900: Invalid channel id ({0})")]
    Transport(String),
    /// The peer exited or the socket closed before answering.
    #[error("E900: Invalid channel id (channel closed while waiting for reply)")]
    PeerGone,
    /// No reply arrived within the blocking budget.
    #[error("E900: Invalid channel id (timed out waiting for rpc reply)")]
    Timeout,
    /// The peer answered with an error: the remote message text.
    #[error("{0}")]
    Remote(String),
}

/// One connected socket channel (`channel_connect`, `channel.c`), owned by
/// [`JobManager`] next to its jobs under the same channel-id space.
struct SocketChannel {
    /// Whether the socket carries msgpack-rpc (`sockconnect(..., {'rpc': 1})`).
    rpc: bool,
    /// The peer went away or the editor closed it.
    closed: bool,
    /// The connected transport.
    stream: SocketStream,
    /// Staged decoder for replies split across reads.
    decoder: IncrementalDecoder,
    /// Bytes read from the transport but not yet fed to [`SocketChannel::decoder`].
    read_buf: Vec<u8>,
    /// Next outbound request id (upstream `rpc->next_request_id`, seeded 1).
    msgid: MsgidCounter,
    /// Replies that arrived while waiting for a different `msgid`.
    stash: VecDeque<(u32, Result<Object, ApiError>)>,
}

/// A connected socket transport behind [`SocketChannel`].
pub enum SocketStream {
    /// Unix-domain socket (`sockconnect('pipe', path)`).
    #[cfg(unix)]
    Pipe(UnixStream),
    /// TCP connection (`sockconnect('tcp', 'host:port')`).
    Tcp(TcpStream),
}

impl SocketChannel {
    /// Write the whole frame or report the transport error.
    fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        match &mut self.stream {
            #[cfg(unix)]
            SocketStream::Pipe(stream) => stream.write_all(data).map_err(|error| error.to_string()),
            SocketStream::Tcp(stream) => stream.write_all(data).map_err(|error| error.to_string()),
        }
    }

    /// One bounded read; an empty read or any error but a timeout ends the
    /// channel, the timeout only means no reply is ready yet.
    fn read_chunk(&mut self, chunk: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.stream {
            #[cfg(unix)]
            SocketStream::Pipe(stream) => stream.read(chunk),
            SocketStream::Tcp(stream) => stream.read(chunk),
        }
    }

    /// Mark the channel closed and release the descriptor promptly, the way
    /// `rstream_may_close` does for sockets.
    fn shutdown(&mut self) {
        self.closed = true;
        match &mut self.stream {
            #[cfg(unix)]
            SocketStream::Pipe(stream) => {
                let _ignored = stream.shutdown(std::net::Shutdown::Both);
            }
            SocketStream::Tcp(stream) => {
                let _ignored = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

#[cfg(unix)]
enum JobInput {
    Pipe(ProcessPipe),
    Pty(PtyHandle),
}

/// Non-Unix spawn hands the job plain `std::process` pipes; anonymous pipes
/// cannot join the loop's readiness reactor there, so a pump thread per
/// direction moves the bytes and `Pipe` is the writer end's ordered queue.
#[cfg(not(unix))]
enum JobInput {
    Pipe(mpsc::Sender<Vec<u8>>),
}

/// What keeps a child stdio pipe delivering: on Unix the loop-registered
/// [`ProcessPipe`] handle must stay alive; elsewhere the pump thread's join
/// handle does.
#[cfg(unix)]
type StdioPump = ProcessPipe;
#[cfg(not(unix))]
type StdioPump = std::thread::JoinHandle<()>;

#[expect(
    clippy::struct_excessive_bools,
    reason = "rpc/detached/terminal/pty are independent channel flags mirrored
              from upstream job options, not states of one machine"
)]
struct Job {
    process: Process,
    input: Option<JobInput>,
    /// Keep-alive plus drain tracking: the pump handle must stay alive for
    /// the stream to deliver, and `drained` consults its presence to know
    /// which streams owe an EOF.
    stdout_pipe: Option<StdioPump>,
    stderr_pipe: Option<StdioPump>,
    callbacks: JobCallbacks,
    stdout: StreamState,
    stderr: StreamState,
    status: i64,
    rpc: bool,
    /// The RPC half was closed (`chanclose(id, 'rpc')`): the job still runs
    /// but no longer answers `rpcnotify`/`rpcrequest` (upstream `rpc_close`).
    rpc_closed: bool,
    /// Raw stdout bytes captured for the RPC request loop. `drain_raw` appends
    /// here for every `rpc` job because unbuffered, callback-less output would
    /// otherwise be discarded before `rpcrequest` could decode its reply.
    rpc_buffer: Vec<u8>,
    /// Staged RPC decoder for the request loop; drained together with
    /// [`Job::rpc_buffer`] so a response split across polls still assembles.
    rpc_decoder: IncrementalDecoder,
    /// Replies that arrived while waiting for a different `msgid`, in wire
    /// order; the next `rpcrequest` consults them before blocking.
    rpc_stash: VecDeque<(u32, Result<Object, ApiError>)>,
    /// Next outbound request id (upstream `rpc->next_request_id`, seeded 1).
    rpc_msgid: MsgidCounter,
    /// PTY slave path the child is attached to, when spawned through a
    /// pseudoterminal. Mirrors upstream's `channel.pty` string populated for
    /// `jobstart(..., {'pty': v:true})` (`eval/funcs.c` `f_jobstart` →
    /// `os/shell.c` `terminal_running`) so `nvim_get_chan_info` can answer the
    /// `pty` field word-for-word.
    pty_slave: Option<String>,
    /// Buffer that receives PTY output, when one was allocated.
    terminal_buffer: Option<ox_types::BufHandle>,
    /// Raw PTY output accumulated for the terminal buffer.
    pty_output: Vec<u8>,
    /// Whether the default `nvim.terminal` `TermClose` handler is active.
    terminal_exit_message: bool,
    /// `jobstart({'detach': v:true})`: upstream leaves a detached child running
    /// past editor exit and terminates every other one (`channel_close_on_exit`).
    detached: bool,
    /// Spawned on a pseudoterminal: its output arrives on the PTY master held
    /// in `input` rather than a `stdout_pipe`, but the stream still owes an
    /// EOF before the job is drained.
    pty: bool,
}

impl Job {
    /// The process exited and every captured stream drained. Reader pumps
    /// queue their final `Data`/`Eof` independently of the exit notification,
    /// so an exited process alone does not imply buffered output is complete.
    /// A PTY child has no `stdout_pipe`/`stderr_pipe` — its output arrives on
    /// the master held in `input` — but still owes a `Stdout` EOF.
    fn drained(&self) -> bool {
        let stdout_owed = self.stdout_pipe.is_some() || self.pty;
        let stderr_owed = self.stderr_pipe.is_some();
        self.status >= 0 && (!stdout_owed || self.stdout.eof) && (!stderr_owed || self.stderr.eof)
    }
}
/// Owns job channels and the `ox-uv` loop which drives their process handles.
pub struct JobManager {
    loop_: UvLoop,
    jobs: HashMap<u64, Job>,
    sockets: HashMap<u64, SocketChannel>,
    raw: Arc<Mutex<VecDeque<RawEvent>>>,
    /// Events polled on a path with no callback host; the next poll re-enters
    /// them (`channel.c` defers callbacks onto the main loop instead of
    /// dropping them, which is what `let _ = poll()` did).
    deferred: Vec<JobEvent>,
    /// Set when the `jobwait` flush re-defers Lua-registered callbacks:
    /// upstream runs them before `jobwait` returns (`multiqueue_process_
    /// events`, funcs.c:3668/3721), so the first borrow-free boundary after
    /// the builtin returns must deliver them, not the next tick.
    lua_flush_pending: bool,
    /// Vimscript `timer_start` timers (`eval.c` `timer_T`), keyed by the
    /// monotonic id `timer_start` answers. They arm [`Timer`] handles on
    /// this same loop — the only loop the editor polls — so firings surface
    /// during `:sleep`, `vim.wait`, and the RPC pump without a second loop.
    timers: HashMap<u64, VimTimer>,
    /// Next `timer_start` id, monotonic from 1 (upstream `last_timer_id`).
    next_timer_id: u64,
    /// Vim timer ids whose loop handles fired since the last poll. The
    /// `Timer` closure runs while the loop is borrowed, so it cannot touch
    /// the timer table; it only records here, and the poll drains the queue
    /// into [`JobEvent`]s once the borrow is free (the same defer-to-a-
    /// borrow-free-boundary shape the channel callbacks use).
    timer_fired: Arc<Mutex<Vec<u64>>>,
}

impl JobManager {
    /// Create an isolated reactor-backed job table.
    ///
    /// # Errors
    ///
    /// Returns the reactor initialization error.
    pub fn new() -> Result<Self, String> {
        let loop_ = UvLoop::new().map_err(|error| error.to_string())?;
        Ok(Self {
            loop_,
            jobs: HashMap::new(),
            sockets: HashMap::new(),
            raw: Arc::new(Mutex::new(VecDeque::new())),
            deferred: Vec::new(),
            lua_flush_pending: false,
            timers: HashMap::new(),
            next_timer_id: 1,
            timer_fired: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Marks a `jobwait` Lua flush as awaiting synchronous delivery.
    pub(crate) fn set_lua_flush_pending(&mut self) {
        self.lua_flush_pending = true;
    }

    /// Takes the `jobwait` Lua-flush marker; the caller that delivers the
    /// flushed callbacks consumes it.
    pub(crate) fn take_lua_flush_pending(&mut self) -> bool {
        std::mem::take(&mut self.lua_flush_pending)
    }

    /// Spawn a process and register it under the already-allocated channel id.
    ///
    /// # Errors
    ///
    /// Returns the process or stream setup error.
    pub fn start(&mut self, id: u64, options: JobStartOptions) -> Result<u32, String> {
        let mut spawn_options = SpawnOptions::new(options.program);
        spawn_options.args = options.args;
        spawn_options.environment = options.environment;
        spawn_options.cwd = options.cwd;
        spawn_options.detached = options.detached;
        spawn_options.stdio = [
            if options.stdin_pipe {
                StdioConfig::CreatePipe
            } else {
                StdioConfig::Ignore
            },
            StdioConfig::CreatePipe,
            StdioConfig::CreatePipe,
        ];

        let exit_queue = Arc::clone(&self.raw);
        let on_exit = move |_loop_: &mut UvLoop, result: process::ProcessExitResult| {
            let result = result.map_err(|error| error.to_string());
            lock_queue(&exit_queue).push_back(RawEvent::Exit(id, result));
        };

        #[cfg(unix)]
        let (process, input, stdout_pipe, stderr_pipe, pty_slave) = if options.pty {
            let mut spawned =
                process::spawn_pty(&mut self.loop_, spawn_options, options.pty_size, on_exit)
                    .map_err(|error| error.to_string())?;
            let output_queue = Arc::clone(&self.raw);
            spawned
                .master
                .read_start(&mut self.loop_, move |_loop_, _handle, event| {
                    queue_stream_event(&output_queue, id, StreamKind::Stdout, event);
                })
                .map_err(|error| error.to_string())?;
            let pty_slave = spawned.pty_slave;
            (
                spawned.process,
                Some(JobInput::Pty(spawned.master)),
                None,
                None,
                pty_slave,
            )
        } else {
            let mut spawned = process::spawn(&mut self.loop_, spawn_options, on_exit)
                .map_err(|error| error.to_string())?;
            let input = spawned.pipes.stdin.take().map(JobInput::Pipe);
            let mut stdout_pipe = spawned.pipes.stdout.take();
            let mut stderr_pipe = spawned.pipes.stderr.take();
            if let Some(pipe) = stdout_pipe.as_mut() {
                let output_queue = Arc::clone(&self.raw);
                pipe.read_start(&mut self.loop_, move |_loop_, _handle, event| {
                    queue_stream_event(&output_queue, id, StreamKind::Stdout, event);
                })
                .map_err(|error| error.to_string())?;
            }
            if let Some(pipe) = stderr_pipe.as_mut() {
                let output_queue = Arc::clone(&self.raw);
                pipe.read_start(&mut self.loop_, move |_loop_, _handle, event| {
                    queue_stream_event(&output_queue, id, StreamKind::Stderr, event);
                })
                .map_err(|error| error.to_string())?;
            }
            (spawned.process, input, stdout_pipe, stderr_pipe, None)
        };

        #[cfg(not(unix))]
        let (process, input, stdout_pipe, stderr_pipe, pty_slave) = {
            if options.pty {
                return Err("jobstart pty is unavailable on this platform".to_owned());
            }
            let mut spawned = process::spawn(&mut self.loop_, spawn_options, on_exit)
                .map_err(|error| error.to_string())?;
            let pumps = (|| {
                let input = spawned
                    .pipes
                    .stdin
                    .take()
                    .map(|stdin| pipe_writer(id, stdin).map(JobInput::Pipe))
                    .transpose()?;
                let stdout_pipe = spawned
                    .pipes
                    .stdout
                    .take()
                    .map(|stream| {
                        pipe_reader(id, StreamKind::Stdout, stream, Arc::clone(&self.raw))
                    })
                    .transpose()?;
                let stderr_pipe = spawned
                    .pipes
                    .stderr
                    .take()
                    .map(|stream| {
                        pipe_reader(id, StreamKind::Stderr, stream, Arc::clone(&self.raw))
                    })
                    .transpose()?;
                Ok::<_, std::io::Error>((input, stdout_pipe, stderr_pipe))
            })();
            match pumps {
                Ok((input, stdout_pipe, stderr_pipe)) => {
                    (spawned.process, input, stdout_pipe, stderr_pipe, None)
                }
                Err(error) => {
                    // The child is not in the job table yet, so teardown
                    // cannot reach it; terminate before reporting failure.
                    let _ = spawned.process.kill(None);
                    return Err(error.to_string());
                }
            }
        };

        let pid = process.pid();
        self.jobs.insert(
            id,
            Job {
                process,
                input,
                stdout_pipe,
                stderr_pipe,
                callbacks: options.callbacks,
                stdout: StreamState {
                    buffered: options.stdout_buffered,
                    ..StreamState::default()
                },
                stderr: StreamState {
                    buffered: options.stderr_buffered,
                    ..StreamState::default()
                },
                status: -1,
                rpc: options.rpc,
                rpc_closed: false,
                rpc_buffer: Vec::new(),
                rpc_decoder: IncrementalDecoder::new(),
                rpc_stash: VecDeque::new(),
                rpc_msgid: MsgidCounter::new(),
                pty_slave,
                terminal_buffer: options.terminal_buffer,
                pty_output: Vec::new(),
                terminal_exit_message: true,
                detached: options.detached,
                pty: options.pty,
            },
        );
        Ok(pid)
    }

    /// Run one non-blocking reactor turn and return callback work queued by
    /// it, ahead of any events deferred by a poll that could not deliver them.
    ///
    /// # Errors
    ///
    /// Returns the reactor polling error.
    pub fn poll(&mut self) -> Result<Vec<JobEvent>, String> {
        self.loop_.run_nowait().map_err(|error| error.to_string())?;
        let mut events = std::mem::take(&mut self.deferred);
        events.extend(self.drain_raw());
        events.extend(self.drain_timers());
        Ok(events)
    }

    /// Stash events a caller polled but could not invoke; the next poll
    /// surfaces them first so callbacks are never silently discarded.
    pub fn defer_events(&mut self, events: Vec<JobEvent>) {
        self.deferred.extend(events);
    }
    /// Takes the whole deferred queue, keeping relative order (the server's
    /// borrow-free `deliver_deferred_job_events` drains before invoking so
    /// no borrow spans user code while the manager stays installed; requeue
    /// goes through [`Self::defer_events`], preserving
    /// `take_deferred_and_invoke`'s handler-deferred-ahead-of-tail order).
    pub fn drain_deferred(&mut self) -> Vec<JobEvent> {
        std::mem::take(&mut self.deferred)
    }
    /// Drain and invoke pending deferred events now.
    ///
    /// The whole queue is drained first (`channel_process_callbacks`,
    /// channel.c:1089-1148): a callback that sends, polls, or stops a job
    /// mutates the job table or enqueues new events, and iterating live state
    /// would see it.
    ///
    /// `invoke` receives the drained batch by `&mut` and removes the events
    /// it invoked from the front (`Vec::drain(..n)`), the same call
    /// chansend/jobwait make on their collected events. Whatever remains in
    /// the vector after `invoke` returns — an uninvoked tail on a handler
    /// failure, exactly what upstream keeps queued (`channel.c` requeues
    /// unprocessed callbacks) — is put back on the queue and surfaces on the
    /// next poll.
    ///
    /// # Errors
    ///
    /// Returns the host's invocation error after requeuing the tail.
    pub fn take_deferred_and_invoke(
        &mut self,
        invoke: impl FnOnce(&mut Vec<JobEvent>) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut events = std::mem::take(&mut self.deferred);
        let result = invoke(&mut events);
        // `invoke` drained what it delivered from the front; anything left is
        // an unconsumed tail (handler failed mid-batch) and stays deliverable.
        self.deferred.extend(events);
        result
    }

    /// Wait for the selected jobs, sharing one deadline across the list.
    ///
    /// # Errors
    ///
    /// Returns the reactor polling error.
    pub fn wait(
        &mut self,
        ids: &[u64],
        timeout_ms: i64,
    ) -> Result<(Vec<i64>, Vec<JobEvent>), String> {
        let deadline = if timeout_ms < 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(timeout_ms.cast_unsigned()))
        };
        let mut events = Vec::new();
        loop {
            match self.poll() {
                Ok(mut polled) => events.append(&mut polled),
                Err(error) => {
                    // Keep the events gathered so far deliverable instead of
                    // dropping them with the error.
                    self.defer_events(events);
                    return Err(error);
                }
            }
            if ids
                .iter()
                .all(|id| self.jobs.get(id).is_none_or(Job::drained))
            {
                break;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let statuses = ids
            .iter()
            .map(|id| self.jobs.get(id).map_or(-3, |job| job.status))
            .collect();
        Ok((statuses, events))
    }

    /// Write raw bytes to the process stdin or PTY master.
    ///
    /// # Errors
    ///
    /// Returns the process input write error.
    pub fn send(&mut self, id: u64, data: Vec<u8>) -> Result<bool, String> {
        let Some(job) = self.jobs.get_mut(&id) else {
            return Ok(false);
        };
        let Some(input) = job.input.as_mut() else {
            return Ok(false);
        };
        match input {
            #[cfg(unix)]
            JobInput::Pipe(pipe) => {
                pipe.write(&mut self.loop_, data)
                    .map_err(|error| error.to_string())?;
            }
            #[cfg(not(unix))]
            JobInput::Pipe(sender) => {
                sender
                    .send(data)
                    .map_err(|_| "job stdin pipe is closed".to_owned())?;
            }
            #[cfg(unix)]
            JobInput::Pty(pty) => {
                pty.write(&mut self.loop_, data)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(true)
    }

    /// Close a job's writable input endpoint so readers observe EOF.
    ///
    /// Dropping the handle is not enough. A [`ProcessPipe`] shares its stream
    /// state with the clone the loop's reactor holds, so letting the handle go
    /// left the child's standard input open: `systemlist('cat', '123')` wrote
    /// its input, dropped the handle, and then waited forever on a `cat` that
    /// never saw EOF. That was the only timeout in oldtest census 3, and the
    /// `/bin/sh -c cat` it left behind outlived the runner's deadline.
    ///
    /// `Handle::close` is what actually deregisters the descriptor and drops it.
    /// A write the synchronous attempt could not finish is still queued, so the
    /// loop is pumped until the queue drains first — the reactor clears it on
    /// either a completed write or a closed peer, so the pump terminates.
    pub fn close_input(&mut self, id: u64) -> bool {
        let Some(job) = self.jobs.get_mut(&id) else {
            return false;
        };
        let Some(input) = job.input.take() else {
            return false;
        };
        match input {
            #[cfg(unix)]
            JobInput::Pipe(pipe) => {
                let deadline = Instant::now() + Duration::from_secs(30);
                while pipe.has_pending_writes() && Instant::now() < deadline {
                    if self.loop_.run_nowait().is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                // `shutdown` refuses while a write is queued, so it could leave
                // the descriptor open on a stuck peer; the full handle close
                // cannot, and a descriptor still open is the whole defect.
                let _ignored = ox_uv::Handle::close(&pipe, &mut self.loop_);
            }
            #[cfg(not(unix))]
            JobInput::Pipe(sender) => {
                // Dropping the sender ends the writer thread once every
                // queued write has flushed, which the child reads as EOF.
                drop(sender);
            }
            // A PTY's master is also its stdin: deregister reads, then
            // dropping the handle hangs the child up.
            #[cfg(unix)]
            JobInput::Pty(mut master) => {
                let _ignored = master.read_stop(&mut self.loop_);
            }
        }
        true
    }

    /// Take output accumulated by buffered streams after the job completes.
    pub fn take_buffered_output(&mut self, id: u64) -> Option<(Vec<u8>, Vec<u8>)> {
        let job = self.jobs.get_mut(&id)?;
        Some((
            std::mem::take(&mut job.stdout.bytes),
            std::mem::take(&mut job.stderr.bytes),
        ))
    }

    /// Send SIGTERM to a live job. An already-reaped job is a successful no-op.
    ///
    /// # Errors
    ///
    /// Returns the process termination error.
    pub fn stop(&mut self, id: u64) -> Result<bool, String> {
        let Some(job) = self.jobs.get(&id) else {
            return Ok(false);
        };
        if job.status < 0 {
            job.process.kill(None).map_err(|error| error.to_string())?;
        }
        Ok(true)
    }

    #[must_use]
    /// Return the child PID for a registered job.
    pub fn pid(&self, id: u64) -> Option<u32> {
        self.jobs.get(&id).map(|job| job.process.pid())
    }

    #[must_use]
    /// Report whether a registered channel carries msgpack-rpc: an `rpc` job
    /// or an `rpc` socket.
    pub fn is_rpc(&self, id: u64) -> bool {
        self.jobs.get(&id).is_some_and(|job| job.rpc)
            || self.sockets.get(&id).is_some_and(|socket| socket.rpc)
    }

    /// Every open RPC channel id, ascending: the `rpcnotify(0, ...)`
    /// broadcast audience.
    #[must_use]
    pub fn rpc_channel_ids(&self) -> Vec<u64> {
        let mut ids = self
            .jobs
            .iter()
            .filter(|(_, job)| job.rpc && !job.rpc_closed)
            .map(|(id, _)| *id)
            .chain(
                self.sockets
                    .iter()
                    .filter(|(_, socket)| socket.rpc && !socket.closed)
                    .map(|(id, _)| *id),
            )
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    /// Whether `id` names the live RPC half of a channel: an `rpc` job whose
    /// RPC side is still open, or an `rpc` socket that is still connected.
    #[must_use]
    pub fn rpc_channel_open(&self, id: u64) -> bool {
        if let Some(job) = self.jobs.get(&id) {
            return job.rpc && !job.rpc_closed;
        }
        self.sockets
            .get(&id)
            .is_some_and(|socket| socket.rpc && !socket.closed)
    }

    /// Close one job stream with upstream `channel_close` semantics
    /// (`channel.c:89-201`): closing stdin on an `rpc` job is rejected with
    /// `e_invstreamrpc` (only the `rpc` part may close it), closing `rpc` on
    /// a job that is not an RPC channel is rejected with `e_invstream`, and
    /// closing a stream of a socket channel is rejected the same way (a
    /// socket has a single stream). Double closes succeed, and an unknown id
    /// reports `e_invchan`.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelCloseError`] for an unknown channel or a stream the
    /// channel does not have.
    pub fn close_job_stream(
        &mut self,
        id: u64,
        part: ChannelPart,
    ) -> Result<bool, ChannelCloseError> {
        if let Some(socket) = self.sockets.get_mut(&id) {
            // A socket has a single stream: closing all of it shuts it down,
            // and closing `rpc` does the same on an RPC socket (`channel.c`
            // `close_main` is set for `kChannelPartRpc` on every stream
            // type — but only an RPC channel has that half, otherwise it is
            // `e_invstream`). Anything else names no stream at all.
            return match part {
                ChannelPart::All => {
                    socket.shutdown();
                    Ok(true)
                }
                ChannelPart::Rpc if socket.rpc => {
                    socket.shutdown();
                    Ok(true)
                }
                _ => Err(ChannelCloseError::InvalidStream),
            };
        }
        let (is_rpc_open, is_pty) = match self.jobs.get(&id) {
            Some(job) => (job.rpc && !job.rpc_closed, job.pty),
            None => return Err(ChannelCloseError::UnknownChannel),
        };
        match part {
            ChannelPart::Stdin | ChannelPart::Stdout if is_rpc_open => {
                return Err(ChannelCloseError::InvalidRpcStream);
            }
            ChannelPart::Rpc => {
                let is_rpc = self.jobs.get(&id).is_some_and(|job| job.rpc);
                if !is_rpc {
                    return Err(ChannelCloseError::InvalidStream);
                }
            }
            _ => {}
        }
        // Reuse the stdin pump drain for pipe inputs: queued writes flush
        // before the descriptor closes, so the child reads them before EOF.
        let close_stdin = matches!(
            part,
            ChannelPart::Stdin | ChannelPart::Rpc | ChannelPart::All
        );
        if close_stdin {
            self.close_input(id);
        }
        // A PTY close hangs the child up, the way closing the master does
        // upstream (`pty_proc_close_master`).
        #[cfg(unix)]
        if is_pty && !matches!(part, ChannelPart::Stdout | ChannelPart::Stderr) {
            if let Some(job) = self.jobs.get(&id) {
                let _ignored = job.process.kill(Some(1));
            }
        }
        // Field-split borrows: the pipes close against the loop while the job
        // entry itself is only flagged, never removed (upstream allows double
        // close and the exit status stays answerable).
        let JobManager { loop_, jobs, .. } = self;
        let Some(job) = jobs.get_mut(&id) else {
            return Err(ChannelCloseError::UnknownChannel);
        };
        match part {
            ChannelPart::Stdin => {}
            ChannelPart::Rpc => {
                job.rpc_closed = true;
            }
            ChannelPart::Stdout => {
                Self::close_pipe(loop_, job.stdout_pipe.take());
                // No more output will arrive: the stream counts as drained
                // without an EOF event.
                job.stdout.eof = true;
            }
            ChannelPart::Stderr => {
                Self::close_pipe(loop_, job.stderr_pipe.take());
                job.stderr.eof = true;
            }
            ChannelPart::All => {
                Self::close_pipe(loop_, job.stdout_pipe.take());
                Self::close_pipe(loop_, job.stderr_pipe.take());
                job.stdout.eof = true;
                job.stderr.eof = true;
                if job.rpc {
                    job.rpc_closed = true;
                }
            }
        }
        Ok(true)
    }

    /// Deregister one stdio pump handle. The job-table entry stays: upstream
    /// allows double close, and the exit status must remain answerable.
    fn close_pipe(loop_: &mut UvLoop, pipe: Option<StdioPump>) {
        if let Some(pipe) = pipe {
            #[cfg(unix)]
            let _ignored = ox_uv::Handle::close(&pipe, loop_);
            #[cfg(not(unix))]
            let _ignored = (pipe, loop_);
        }
    }

    /// Register a connected socket channel under the already-allocated id.
    pub fn add_socket(&mut self, id: u64, stream: SocketStream, rpc: bool) {
        self.sockets.insert(
            id,
            SocketChannel {
                rpc,
                closed: false,
                stream,
                decoder: IncrementalDecoder::new(),
                read_buf: Vec::new(),
                msgid: MsgidCounter::new(),
                stash: VecDeque::new(),
            },
        );
    }

    /// Write raw bytes to a socket channel, as `chansend` does for jobs.
    ///
    /// # Errors
    ///
    /// Returns the transport write error; a missing or closed socket reports
    /// `Ok(false)` like a missing job writer does.
    pub fn socket_send(&mut self, id: u64, data: &[u8]) -> Result<bool, String> {
        let Some(socket) = self.sockets.get_mut(&id) else {
            return Ok(false);
        };
        if socket.closed {
            return Ok(false);
        }
        socket.write_all(data).map(|()| true)
    }

    /// Next outbound request id for the channel (upstream
    /// `rpc->next_request_id`, seeded 1). A closed RPC half issues no ids:
    /// `chanclose(id, 'rpc')` retires the channel for `rpcrequest`.
    pub fn rpc_next_msgid(&mut self, id: u64) -> Option<u32> {
        if let Some(job) = self.jobs.get_mut(&id) {
            return (job.rpc && !job.rpc_closed).then(|| job.rpc_msgid.next_id());
        }
        self.sockets.get_mut(&id).and_then(|socket| {
            (socket.rpc && !socket.closed).then(|| socket.msgid.next_id())
        })
    }

    /// Whether `id` names a `sockconnect` socket channel rather than a job.
    #[must_use]
    pub fn is_socket_channel(&self, id: u64) -> bool {
        self.sockets.contains_key(&id)
    }

    /// Send an RPC notification frame over a job or socket channel.
    ///
    /// # Errors
    ///
    /// Returns the encode or transport error; a missing or non-RPC channel
    /// reports `Ok(false)`.
    pub fn rpc_notify(
        &mut self,
        id: u64,
        method: &OxStr,
        params: Vec<Object>,
    ) -> Result<bool, String> {
        let frame = Message::Notification {
            method: method.clone(),
            params,
        }
        .encode_bytes()
        .map_err(|error| error.to_string())?;
        if self.jobs.contains_key(&id) {
            if !self.rpc_channel_open(id) {
                return Ok(false);
            }
            return self.send(id, frame);
        }
        self.socket_send(id, &frame)
    }

    /// Send an RPC request and block until its reply arrives (upstream
    /// `rpc_send_call`, which waits on the main loop). Responses for other
    /// `msgid`s are stashed for the next call. Callback events that surface
    /// while waiting are re-deferred, never delivered on this stack.
    ///
    /// # Errors
    ///
    /// Returns the encode, transport, decode, or timeout error, or the
    /// channel closing before its reply arrives.
    pub fn rpc_call(
        &mut self,
        id: u64,
        method: &OxStr,
        params: Vec<Object>,
        timeout: Duration,
    ) -> Result<Object, RpcCallError> {
        let Some(msgid) = self.rpc_next_msgid(id) else {
            return Err(RpcCallError::NotRpc);
        };
        let frame = Message::Request {
            msgid,
            method: method.clone(),
            params,
        }
        .encode_bytes()
        .map_err(|error| RpcCallError::Encode(error.to_string()))?;
        // A closed write side (`chanclose(id, 'stdin')`) answers `Ok(false)`
        // instead of erroring: the peer is gone, so fail fast rather than
        // waiting out the deadline for a reply that cannot arrive.
        let sent = if self.jobs.contains_key(&id) {
            self.send(id, frame)
                .map_err(RpcCallError::Transport)?
        } else {
            self.socket_send(id, &frame)
                .map_err(RpcCallError::Transport)?
        };
        if !sent {
            return Err(RpcCallError::PeerGone);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(reply) = self.take_rpc_reply(id, msgid) {
                return reply;
            }
            if self.rpc_peer_gone(id) {
                return Err(RpcCallError::PeerGone);
            }
            if Instant::now() >= deadline {
                return Err(RpcCallError::Timeout);
            }
            self.pump_rpc(id).map_err(RpcCallError::Transport)?;
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Pop the reply for `msgid` off the channel, decoding newly arrived bytes
    /// first. Non-matching responses are stashed in wire order.
    fn take_rpc_reply(
        &mut self,
        id: u64,
        msgid: u32,
    ) -> Option<Result<Object, RpcCallError>> {
        let (stashed, decoder, buffered) = if let Some(job) = self.jobs.get_mut(&id) {
            (
                &mut job.rpc_stash,
                &mut job.rpc_decoder,
                std::mem::take(&mut job.rpc_buffer),
            )
        } else if let Some(socket) = self.sockets.get_mut(&id) {
            (
                &mut socket.stash,
                &mut socket.decoder,
                std::mem::take(&mut socket.read_buf),
            )
        } else {
            return Some(Err(RpcCallError::NotRpc));
        };
        let feed = decoder.feed(&buffered);
        let (messages, decode_error) = match feed {
            Ok(messages) => (messages, None),
            Err(error) => (error.messages, Some(error.error.to_string())),
        };
        for message in messages {
            if let Message::Response {
                msgid: got,
                result,
            } = message
            {
                            if got == msgid {
                return Some(result.map_err(|error| RpcCallError::Remote(error.to_string())));
            }
            if stashed.len() >= MAX_RPC_STASH {
                stashed.clear();
                return Some(Err(RpcCallError::Transport(
                    "too many pending rpc responses".to_owned(),
                )));
            }
            stashed.push_back((got, result));
            }
            // Requests and notifications addressed at us have no host on
            // this stack; upstream dispatches them on the main loop, so
            // they are dropped here rather than answered wrongly.
        }
        if let Some(error) = decode_error {
            return Some(Err(RpcCallError::Transport(error)));
        }
        if let Some(position) = stashed.iter().position(|(got, _)| *got == msgid) {
            return stashed.remove(position).map(|(_, reply)| {
                reply.map_err(|error| RpcCallError::Remote(error.to_string()))
            });
        }
        None
    }

    /// Whether the RPC peer is gone: the job exited and its output drained,
    /// or the socket closed or errored.
    fn rpc_peer_gone(&mut self, id: u64) -> bool {
        if let Some(job) = self.jobs.get(&id) {
            return job.status >= 0 && job.drained();
        }
        self.sockets.get(&id).is_none_or(|socket| socket.closed)
    }

    /// Pump one transport turn so a pending reply can arrive: one non-blocking
    /// reactor turn (re-deferring callback events, which also drives due
    /// timers so they cannot starve while a request waits), then a
    /// timeout-bounded read for socket transports.
    fn pump_rpc(&mut self, id: u64) -> Result<(), String> {
        let events = self.poll()?;
        self.defer_events(events);
        if self.jobs.contains_key(&id) {
            return Ok(());
        }
        let Some(socket) = self.sockets.get_mut(&id) else {
            return Ok(());
        };
        if socket.closed {
            return Ok(());
        }
        let mut chunk = [0_u8; 8192];
        match socket.read_chunk(&mut chunk) {
            Ok(0) => {
                socket.shutdown();
            }
            Ok(read) => {
                if socket.read_buf.len() + read > MAX_RPC_BUFFER {
                    socket.read_buf.clear();
                    socket.shutdown();
                } else {
                    socket.read_buf.extend_from_slice(&chunk[..read]);
                }
            }
            Err(error) => {
                // A read timeout only means no reply yet; anything else ends
                // the channel the way an EOF does.
                if error.kind() != std::io::ErrorKind::TimedOut
                    && error.kind() != std::io::ErrorKind::WouldBlock
                {
                    socket.shutdown();
                }
            }
        }
        Ok(())
    }

    /// PTY slave path behind a job channel that was spawned through one.
    #[must_use]
    pub fn pty_slave(&self, id: u64) -> Option<&str> {
        self.jobs.get(&id).and_then(|job| job.pty_slave.as_deref())
    }

    /// Bind the editor-owned terminal buffer to an already-started PTY job.
    pub fn set_terminal_buffer(&mut self, id: u64, buffer: ox_types::BufHandle) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.terminal_buffer = Some(buffer);
        }
    }

    /// The terminal job projecting into `buffer`, if any, with whether it
    /// is still pending (`terminal_running`, os/shell.c): a pending channel
    /// rejects a second attach, a completed one is closed and reused
    /// (`f_jobstart`, eval/funcs.c:3491-3498).
    #[must_use]
    pub fn terminal_job_for_buffer(&self, buffer: ox_types::BufHandle) -> Option<(u64, bool)> {
        self.jobs.iter().find_map(|(id, job)| {
            (job.terminal_buffer == Some(buffer)).then_some((*id, job.status == -1))
        })
    }

    /// Drop a completed job's terminal binding so its buffer is reusable.
    pub fn clear_terminal_buffer(&mut self, id: u64) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.terminal_buffer = None;
        }
    }

    /// Enable or disable the default terminal process-exit message.
    pub fn set_terminal_exit_message(&mut self, id: u64, enabled: bool) {
        if let Some(job) = self.jobs.get_mut(&id) {
            job.terminal_exit_message = enabled;
        }
    }

    /// Take accumulated PTY output for the terminal buffer.
    pub fn take_pty_output(&mut self, id: u64) -> Option<Vec<u8>> {
        let job = self.jobs.get_mut(&id)?;
        if job.pty_output.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut job.pty_output))
    }

    /// Poll once and write any PTY output to the editor-owned terminal buffer.
    ///
    /// # Errors
    ///
    /// Returns the reactor polling or terminal-buffer update error.
    pub fn flush_pty_to_editor(
        &mut self,
        id: u64,
        editor: &mut crate::Editor,
    ) -> Result<(), String> {
        let events = self.poll()?;
        self.defer_events(events);
        if let Some(bytes) = self.take_pty_output(id) {
            editor
                .append_terminal_buffer(id, &bytes)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Poll once and take pending output for every terminal-backed job.
    ///
    /// Callback events remain deferred for the normal callback host.
    ///
    /// # Errors
    ///
    /// Returns the reactor polling error.
    pub fn take_all_pty_output(&mut self) -> Result<Vec<(u64, Vec<u8>)>, String> {
        let events = self.poll()?;
        self.defer_events(events);
        let output = self
            .jobs
            .iter_mut()
            .filter_map(|(&id, job)| {
                (!job.pty_output.is_empty()).then(|| (id, std::mem::take(&mut job.pty_output)))
            })
            .collect::<Vec<_>>();
        let cursor_queries = output
            .iter()
            .filter_map(|(id, bytes)| {
                bytes
                    .windows(4)
                    .any(|bytes| bytes == b"\x1b[6n")
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in cursor_queries {
            self.send(id, b"\x1b[1;1R".to_vec())?;
        }
        Ok(output)
    }

    fn drain_raw(&mut self) -> Vec<JobEvent> {
        let raw = {
            let mut queue = lock_queue(&self.raw);
            queue.drain(..).collect::<Vec<_>>()
        };
        let mut callbacks = Vec::new();
        for event in raw {
            match event {
                RawEvent::Data(id, stream, bytes) => {
                    let Some(job) = self.jobs.get_mut(&id) else {
                        continue;
                    };
                    // PTY output accumulates for the terminal buffer.
                    if job.terminal_buffer.is_some() && matches!(stream, StreamKind::Stdout) {
                        job.pty_output.extend_from_slice(&bytes);
                    }
                    // RPC output accumulates for the request loop: unbuffered,
                    // callback-less bytes would otherwise be discarded before
                    // `rpcrequest` decodes its reply. A flooded channel closes
                    // its RPC half instead of growing without bound.
                    if job.rpc && !job.rpc_closed && matches!(stream, StreamKind::Stdout) {
                        if job.rpc_buffer.len() + bytes.len() > MAX_RPC_BUFFER {
                            job.rpc_buffer.clear();
                            job.rpc_closed = true;
                        } else {
                            job.rpc_buffer.extend_from_slice(&bytes);
                        }
                    }
                    let (state, callback, name) = stream_parts(job, stream);
                    if state.buffered {
                        state.bytes.extend_from_slice(&bytes);
                    } else if let Some(callback) = callback {
                        callbacks.push(data_event(
                            id,
                            callback,
                            &job.callbacks.options,
                            name,
                            &bytes,
                        ));
                    }
                }
                RawEvent::Eof(id, stream) => {
                    let Some(job) = self.jobs.get_mut(&id) else {
                        continue;
                    };
                    let (state, callback, name) = stream_parts(job, stream);
                    if state.eof {
                        continue;
                    }
                    state.eof = true;
                    if let Some(callback) = callback {
                        let bytes = if state.buffered {
                            std::mem::take(&mut state.bytes)
                        } else {
                            Vec::new()
                        };
                        callbacks.push(data_event(
                            id,
                            callback,
                            &job.callbacks.options,
                            name,
                            &bytes,
                        ));
                    }
                }
                RawEvent::Exit(id, result) => {
                    let Some(job) = self.jobs.get_mut(&id) else {
                        continue;
                    };
                    let status = match result {
                        Ok(exit) if exit.signal != 0 => i64::from(128 + exit.signal),
                        Ok(exit) => exit.code,
                        Err(_) => -2,
                    };
                    job.status = status;
                    if job.terminal_buffer.is_some() && job.terminal_exit_message {
                        job.pty_output.extend_from_slice(
                            format!("\r\n[Process exited {status}]\r\n").as_bytes(),
                        );
                    }
                    if let Some(callback) = job.callbacks.exit.clone() {
                        callbacks.push(JobEvent {
                            callback,
                            receiver: job.callbacks.options.clone(),
                            args: vec![
                                Typval::Number(id.cast_signed()),
                                Typval::Number(status),
                                Typval::String(OxStr::from("exit")),
                            ],
                        });
                    }
                }
            }
        }
        callbacks
    }
}

/// No non-detached child outlives the manager that spawned it.
///
/// Upstream terminates every job channel on exit and leaves only `detach`ed
/// ones running (`channel_close_on_exit`). Without this a child still blocked
/// when the editor goes away becomes an orphan holding the inherited standard
/// output, which is what made the census's `test_system.vim` timeout hang a
/// runner reading that pipe rather than just failing.
impl Drop for JobManager {
    fn drop(&mut self) {
        for job in self.jobs.values() {
            if job.status < 0 && !job.detached {
                let _ignored = ox_uv::Handle::close(&job.process, &mut self.loop_);
            }
        }
    }
}

/// One Vimscript `timer_start` timer (upstream `eval.c` `timer_T`).
///
/// The loop handle fires on [`JobManager::loop_`], so every reactor pump —
/// `:sleep`, `vim.wait`, the RPC wait — drives it. The handle closure never
/// runs Vimscript: it only records the firing, and [`JobManager::poll`]
/// turns the record into a [`JobEvent`] on a borrow-free boundary.
struct VimTimer {
    /// Loop handle armed for the next firing while the timer is live.
    handle: Timer,
    /// Requested delay in milliseconds, kept verbatim for `timer_info`
    /// (upstream stores the raw `timeout`, including negatives, which arm
    /// the watcher with a wrapped-around delay that effectively never
    /// fires; this port leaves such timers unarmed instead).
    timeout_ms: i64,
    /// Firings still owed; negative repeats forever. `repeat == 0` is
    /// normalized to 1 at start (upstream `f_timer_start`).
    repeat_remaining: i64,
    /// Paused timers hold no armed handle until resumed (upstream
    /// `f_timer_pause` stops/starts the watcher).
    paused: bool,
    /// Whether the handle is currently armed. A `TimeOverflow` timeout
    /// (negative or absurd `time`) leaves a valid unarmed entry that shows
    /// in `timer_info`, never fires, and still stops cleanly.
    armed: bool,
    /// Callback invoked with the timer id on every firing.
    callback: Typval,
    /// Callback failures so far; three strikes stop the timer (upstream
    /// `emsg_count` in `timer_due_cb`).
    emsg_count: u32,
    /// Set by `timer_stop`. Upstream only marks the timer stopped and frees
    /// it from `timer_close_cb`, so a stopped timer that still has a delivery
    /// in flight keeps its entry (`eval.c:5153-5180`).
    stopped: bool,
    /// A firing of this timer is being delivered. Upstream's `refcount` is 2
    /// for the duration of `timer_due_cb`, which is what keeps a stopped
    /// timer visible to `timer_info` while its callback runs (`eval.c:5092`,
    /// `5074-5078`).
    in_flight: bool,
    /// The `self` dictionary cloned into every firing's [`JobEvent`]. One
    /// identity per timer, so [`JobManager::timer_id_for_event`] recognizes
    /// timer firings by `Rc::ptr_eq` instead of by argument shape — a job
    /// callback can never present this identity, however its arguments
    /// happen to line up.
    receiver: DictRef,
}

impl JobManager {
    /// Start a Vimscript timer on the job loop, answering its monotonic id.
    ///
    /// `repeat == 0` means 1; negative repeats forever. The callback is
    /// stored as given and cloned into one [`JobEvent`] per firing.
    ///
    /// # Errors
    ///
    /// Returns the loop handle error when no timer handle can be allocated.
    pub fn timer_start(
        &mut self,
        timeout_ms: i64,
        repeat: i64,
        callback: Typval,
    ) -> Result<u64, String> {
        let id = self.next_timer_id;
        self.next_timer_id = self.next_timer_id.wrapping_add(1).max(1);
        let handle = Timer::new(&mut self.loop_).map_err(|error| error.to_string())?;
        // Only handle exhaustion fails here; an unrepresentable timeout
        // (negative or absurd `time`) keeps a valid unarmed entry that shows
        // in `timer_info`, never fires, and still stops cleanly.
        let armed = Self::arm_timer(
            &mut self.loop_,
            &self.timer_fired,
            &handle,
            id,
            timeout_ms,
        );
        let receiver = match Typval::dict(Vec::new()) {
            Typval::Dict(reference) => reference,
            _ => unreachable!("Typval::dict builds a Dict"),
        };
        self.timers.insert(
            id,
            VimTimer {
                handle,
                timeout_ms,
                repeat_remaining: if repeat == 0 { 1 } else { repeat },
                paused: false,
                armed,
                callback,
                emsg_count: 0,
                stopped: false,
                in_flight: false,
                receiver,
            },
        );
        Ok(id)
    }

    /// Stop the timer, silently ignoring unknown ids (upstream `f_timer_stop`
    /// returns without touching `rettv` when the id is unknown).
    pub fn timer_stop(&mut self, id: u64) {
        let Some(timer) = self.timers.get_mut(&id) else {
            return;
        };
        let _ignored = timer.handle.stop(&mut self.loop_);
        let _ignored = ox_uv::Handle::close(&timer.handle, &mut self.loop_);
        timer.armed = false;
        timer.stopped = true;
        // A timer stopped from inside its own callback stays listed until the
        // delivery finishes, then `timer_delivered` frees it — upstream's
        // async `timer_close_cb` (`eval.c:5165-5174`).
        if !timer.in_flight {
            self.timers.remove(&id);
        }
    }

    /// Marks one timer firing as delivered, freeing a stopped timer whose
    /// last reference has gone (upstream `timer_decref` -> `timer_close_cb`).
    pub fn timer_delivered(&mut self, id: u64) {
        let Some(timer) = self.timers.get_mut(&id) else {
            return;
        };
        timer.in_flight = false;
        if timer.stopped {
            self.timers.remove(&id);
        }
    }

    /// Stop every timer (upstream `timer_stop_all`).
    pub fn timer_stop_all(&mut self) {
        let ids: Vec<u64> = self.timers.keys().copied().collect();
        for id in ids {
            // Same rule as `timer_stop`: a timer whose firing is in flight
            // stays listed until the delivery ends, so a callback that stops
            // every timer still sees itself (`eval.c:5074-5078, 5153-5180`).
            self.timer_stop(id);
        }
    }

    /// Pause or resume the timer, silently ignoring unknown ids. Resuming
    /// re-arms the handle with the original timeout (upstream `f_timer_pause`
    /// restarts the watcher with `(timeout, timeout)`).
    pub fn timer_pause(&mut self, id: u64, paused: bool) {
        let JobManager {
            loop_,
            timers,
            timer_fired,
            ..
        } = self;
        let Some(timer) = timers.get_mut(&id) else {
            return;
        };
        if timer.paused == paused {
            return;
        }
        timer.paused = paused;
        if paused {
            let _ignored = timer.handle.stop(loop_);
        } else if timer.armed {
            timer.armed = Self::arm_timer(loop_, timer_fired, &timer.handle, id, timer.timeout_ms);
        }
    }

    /// Render `timer_info([id])` (upstream `add_timer_info`): one dict per
    /// live timer with `id`, `time`, `paused`, `repeat`, and `callback`
    /// entries, in that order; empty when no timer matches.
    #[must_use]
    pub fn timer_info(&self, id: Option<u64>) -> Typval {
        let mut ids: Vec<u64> = self.timers.keys().copied().collect();
        ids.sort_unstable();
        let mut items = Vec::with_capacity(ids.len());
        for timer_id in ids {
            if id.is_some_and(|wanted| wanted != timer_id) {
                continue;
            }
            // A stopped timer is listed only while a firing is in flight
            // (upstream `!stopped || refcount > 1`).
            if self
                .timers
                .get(&timer_id)
                .is_some_and(|timer| timer.stopped && !timer.in_flight)
            {
                continue;
            }
            // The id came from the live key set, so the entry exists.
            let Some(timer) = self.timers.get(&timer_id) else {
                continue;
            };
            items.push(Typval::dict(vec![
                (
                    OxStr::from("id"),
                    Typval::Number(timer_id.cast_signed()),
                ),
                (OxStr::from("time"), Typval::Number(timer.timeout_ms)),
                (
                    OxStr::from("paused"),
                    Typval::Number(i64::from(timer.paused)),
                ),
                (
                    OxStr::from("repeat"),
                    Typval::Number(if timer.repeat_remaining < 0 {
                        -1
                    } else {
                        timer.repeat_remaining
                    }),
                ),
                (
                    OxStr::from("callback"),
                    timer.callback.clone(),
                ),
            ]));
        }
        Typval::list(items)
    }

    /// Identify a live timer firing among delivered [`JobEvent`]s by
    /// receiver identity: every firing carries its timer's [`VimTimer`]
    /// dictionary, and no other event can present that `Rc`. Stopped timers
    /// leave the table, so their stale in-flight firings no longer match.
    /// Delivery sites call this before the event is consumed, then
    /// [`Self::record_timer_error`].
    #[must_use]
    pub fn timer_id_for_event(&self, event: &JobEvent) -> Option<u64> {
        self.timers.iter().find_map(|(id, timer)| {
            Rc::ptr_eq(&timer.receiver, &event.receiver).then_some(*id)
        })
    }

    /// Record one failed timer-callback delivery: counts the failure and
    /// returns whether the event was a live timer firing. Delivery sites
    /// (the `jobwait` flush, the server tick, direct invokes) call this
    /// with the event when its callback fails, before the event is
    /// consumed; job events pass through untouched.
    pub fn record_timer_event_error(&mut self, event: &JobEvent) -> bool {
        if let Some(id) = self.timer_id_for_event(event) {
            self.record_timer_error(id);
            true
        } else {
            false
        }
    }

    /// Record one failed timer-callback invocation; three strikes stop the
    /// timer (upstream `emsg_count` in `timer_due_cb`). The shared job-event
    /// delivery reports the failure itself; this only does the counting.
    pub fn record_timer_error(&mut self, id: u64) {
        let mut stop = false;
        if let Some(timer) = self.timers.get_mut(&id) {
            timer.emsg_count = timer.emsg_count.saturating_add(1);
            stop = timer.emsg_count >= 3;
        }
        if stop {
            self.timer_stop(id);
        }
    }

    /// Arm (or re-arm) one timer handle with the firing record closure.
    /// Returns whether the handle is armed; an unrepresentable timeout
    /// reports false instead of failing, so the entry stays valid but idle.
    fn arm_timer(
        loop_: &mut UvLoop,
        fired: &Arc<Mutex<Vec<u64>>>,
        handle: &Timer,
        id: u64,
        timeout_ms: i64,
    ) -> bool {
        let Ok(timeout) = u64::try_from(timeout_ms) else {
            return false;
        };
        let queue = Arc::clone(fired);
        handle
            .start(loop_, timeout, timeout, move |_, _| {
                // Blockable like upstream's `time_watcher` (`tw.blockable`):
                // while the loop is blocked, at most one firing waits per
                // timer instead of one per elapsed period.
                if let Ok(mut queue) = queue.lock() {
                    if !queue.contains(&id) {
                        queue.push(id);
                    }
                }
                Ok(())
            })
            .is_ok()
    }

    /// Turn loop-handle firings recorded since the last poll into [`JobEvent`]s.
    ///
    /// Repeat bookkeeping happens here, at fire time (upstream `timer_due_cb`
    /// decrements `repeat_count` and stops the timer before `callback_call`):
    /// the final firing still delivers its callback, and a finished timer
    /// leaves the table so `timer_info` no longer lists it. A zero-timeout
    /// repeater re-arms every turn (upstream's `timeout == 0` restart); a
    /// nonzero repeater rides the handle's own repeat.
    fn drain_timers(&mut self) -> Vec<JobEvent> {
        let fired: Vec<u64> = self
            .timer_fired
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default();
        if fired.is_empty() {
            return Vec::new();
        }
        let mut events = Vec::with_capacity(fired.len());
        for id in fired {
            // The firing is in flight before repeat bookkeeping runs: upstream
            // raises `refcount` first, so the `timer_stop` a last repeat
            // performs only marks the timer and the callback still sees it in
            // `timer_info` (`eval.c:5092-5116`).
            if let Some(timer) = self.timers.get_mut(&id) {
                timer.in_flight = true;
            }
            let (callback, receiver) = match self.timers.get_mut(&id) {
                Some(timer) if !timer.paused => {
                    if timer.repeat_remaining >= 0 {
                        timer.repeat_remaining -= 1;
                    }
                    (timer.callback.clone(), timer.receiver.clone())
                }
                _ => continue,
            };
            let finished = self
                .timers
                .get(&id)
                .is_some_and(|timer| timer.repeat_remaining == 0);
            if finished {
                self.timer_stop(id);
            } else if self
                .timers
                .get(&id)
                .is_some_and(|timer| timer.armed && timer.timeout_ms == 0)
            {
                let JobManager {
                    loop_,
                    timers,
                    timer_fired,
                    ..
                } = self;
                if let Some(timer) = timers.get_mut(&id) {
                    let queue = Arc::clone(timer_fired);
                    timer.armed = timer
                        .handle
                        .start(loop_, 0, 0, move |_, _| {
                            if let Ok(mut queue) = queue.lock() {
                                if !queue.contains(&id) {
                                    queue.push(id);
                                }
                            }
                            Ok(())
                        })
                        .is_ok();
                }
            }
            events.push(JobEvent {
                callback,
                receiver,
                args: vec![Typval::Number(id.cast_signed())],
            });
        }
        events
    }
}

fn stream_parts(
    job: &mut Job,
    stream: StreamKind,
) -> (&mut StreamState, Option<Typval>, &'static str) {
    match stream {
        StreamKind::Stdout => (&mut job.stdout, job.callbacks.stdout.clone(), "stdout"),
        StreamKind::Stderr => (&mut job.stderr, job.callbacks.stderr.clone(), "stderr"),
    }
}

fn data_event(
    id: u64,
    callback: Typval,
    receiver: &DictRef,
    name: &'static str,
    bytes: &[u8],
) -> JobEvent {
    let lines = if bytes.is_empty() {
        vec![Typval::String(OxStr(Vec::new()))]
    } else {
        bytes
            .split(|byte| *byte == b'\n')
            .map(|line| Typval::String(OxStr(line.strip_suffix(b"\r").unwrap_or(line).to_vec())))
            .collect()
    };
    JobEvent {
        callback,
        receiver: receiver.clone(),
        args: vec![
            Typval::Number(id.cast_signed()),
            Typval::list(lines),
            Typval::String(OxStr::from(name)),
        ],
    }
}

/// Pumps one child pipe onto the raw-event queue from a dedicated thread.
///
/// Anonymous pipes on this platform offer no reactor-safe nonblocking mode,
/// so a blocking reader emits the same `Data`/`Eof` sequence a
/// [`ProcessPipe`] readiness delivery produces on Unix. The read loop ends
/// when the child's write end dies with the process or the descriptor
/// errors, and the [`Job`] retains the join handle so output cannot be
/// orphaned before teardown reaps the process.
#[cfg(not(unix))]
fn pipe_reader(
    id: u64,
    stream: StreamKind,
    mut reader: impl std::io::Read + Send + 'static,
    queue: Arc<Mutex<VecDeque<RawEvent>>>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let name = match stream {
        StreamKind::Stdout => "stdout",
        StreamKind::Stderr => "stderr",
    };
    std::thread::Builder::new()
        .name(format!("ox-job-{id}-{name}"))
        .spawn(move || {
            let mut buffer = vec![0_u8; 8 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        lock_queue(&queue).push_back(RawEvent::Data(
                            id,
                            stream,
                            buffer[..read].to_vec(),
                        ));
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            lock_queue(&queue).push_back(RawEvent::Eof(id, stream));
        })
}

/// Drains an ordered send queue onto child stdin from a dedicated thread.
///
/// A blocking `ChildStdin` write can wait on a full pipe buffer; holding it
/// on this thread keeps [`JobManager::send`] non-blocking. When the sender
/// drops — [`JobManager::close_input`] or job teardown — `recv` delivers
/// the queued writes first, then `Err`, so the child receives exactly the
/// flushed input before stdin closes.
#[cfg(not(unix))]
fn pipe_writer(id: u64, mut stdin: ChildStdin) -> std::io::Result<mpsc::Sender<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name(format!("ox-job-{id}-stdin"))
        .spawn(move || {
            while let Ok(data) = receiver.recv() {
                if stdin.write_all(&data).is_err() {
                    break;
                }
            }
        })
        .map(|_| sender)
}

#[cfg(unix)]
fn queue_stream_event(
    queue: &Arc<Mutex<VecDeque<RawEvent>>>,
    id: u64,
    stream: StreamKind,
    event: NetEvent,
) {
    let event = match event {
        NetEvent::Read(bytes) => Some(RawEvent::Data(id, stream, bytes)),
        NetEvent::Eof | NetEvent::Error(_) => Some(RawEvent::Eof(id, stream)),
        _ => None,
    };
    if let Some(event) = event {
        lock_queue(queue).push_back(event);
    }
}

fn lock_queue(
    queue: &Arc<Mutex<VecDeque<RawEvent>>>,
) -> std::sync::MutexGuard<'_, VecDeque<RawEvent>> {
    queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(test, unix))]
#[expect(
    clippy::unwrap_used,
    reason = "job tests use unwrap to fail immediately on fixture setup errors"
)]
mod tests {
    use super::*;

    fn options(command: &str, buffered: bool) -> JobStartOptions {
        let options = Typval::dict(Vec::new());
        let Typval::Dict(reference) = options else {
            unreachable!()
        };
        let callback = Typval::String(OxStr::from("Callback"));
        JobStartOptions {
            program: PathBuf::from("sh"),
            args: vec![OsString::from("-c"), OsString::from(command)],
            environment: None,
            cwd: None,
            detached: false,
            pty: false,
            term: false,
            pty_size: DEFAULT_PTY_SIZE,
            rpc: false,
            stdin_pipe: true,
            stdout_buffered: buffered,
            stderr_buffered: buffered,
            terminal_buffer: None,
            callbacks: JobCallbacks {
                options: reference,
                stdout: Some(callback.clone()),
                stderr: Some(callback.clone()),
                exit: Some(callback),
            },
        }
    }

    /// Buffered with no callbacks, which is how `system()`/`systemlist()`
    /// configure a job: `drain_raw` hands a buffered stream's bytes to the EOF
    /// callback when there is one, so only this shape leaves them for
    /// `take_buffered_output`.
    fn collected(command: &str) -> JobStartOptions {
        let Typval::Dict(reference) = Typval::dict(Vec::new()) else {
            unreachable!()
        };
        JobStartOptions {
            callbacks: JobCallbacks {
                options: reference,
                stdout: None,
                stderr: None,
                exit: None,
            },
            ..options(command, true)
        }
    }

    fn event_name(event: &JobEvent) -> &str {
        match event.args.get(2) {
            Some(Typval::String(name)) => match name.as_bytes() {
                b"stdout" => "stdout",
                b"stderr" => "stderr",
                b"exit" => "exit",
                _ => "unknown",
            },
            _ => "unknown",
        }
    }

    #[test]
    fn reap_error_marks_terminal_buffer_reusable() {
        let mut jobs = JobManager::new().unwrap();
        let buffer = ox_types::BufHandle::try_from(7).unwrap();
        let mut job_options = options("sleep 10", true);
        job_options.terminal_buffer = Some(buffer);
        jobs.start(3, job_options).unwrap();
        assert_eq!(jobs.terminal_job_for_buffer(buffer), Some((3, true)));
        lock_queue(&jobs.raw).push_back(RawEvent::Exit(3, Err("reap failed".to_owned())));

        jobs.poll().unwrap();

        assert_eq!(jobs.terminal_job_for_buffer(buffer), Some((3, false)));
    }

    #[test]
    fn buffered_output_and_exit_are_deferred_through_the_loop() {
        let mut jobs = JobManager::new().unwrap();
        jobs.start(3, options("printf 'alpha\nbeta'; printf 'err' >&2", true))
            .unwrap();
        let (status, events) = jobs.wait(&[3], 2_000).unwrap();
        assert_eq!(status, vec![0]);
        assert!(events.iter().any(|event| event_name(event) == "stdout"));
        assert!(events.iter().any(|event| event_name(event) == "stderr"));
        assert!(events.iter().any(|event| event_name(event) == "exit"));
    }

    // system()'s contract: events drained while waiting for one job must
    // stay deliverable for the others. run_shell_command re-defers what
    // wait() collected; this pins that the deferred queue survives the
    // round-trip and surfaces on a later poll.
    #[test]
    fn redeferred_events_survive_a_wait_for_another_job() {
        let mut jobs = JobManager::new().unwrap();
        jobs.start(3, options("printf 'out'", true)).unwrap();
        jobs.start(4, collected("printf 'other'")).unwrap();
        let (_, events) = jobs.wait(&[3], 2_000).unwrap();
        jobs.defer_events(events);
        let mut surfaced = false;
        for _ in 0..100 {
            let polled = jobs.poll().unwrap();
            // exit can lead stdout in the re-deferred queue; any position
            // counts — the pin is that the event surfaces, not its order.
            if polled.iter().any(|event| event_name(event) == "stdout") {
                surfaced = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        jobs.wait(&[4], 2_000).unwrap();
        assert!(surfaced, "re-deferred events must surface on a later poll");
    }

    // `close_input` must close the descriptor, not just drop the handle: the
    // loop holds its own clone of the stream state, so dropping alone left the
    // child's standard input open. `cat` is the exact shape that exposes it —
    // it reads to EOF, so without the close it never exits and the bounded
    // wait below reports the timeout sentinel instead of a status.
    //
    // Three parts, each isolating one: the child must exit (a leaked
    // descriptor leaves it running), its status must be the real 0 (a killed
    // child would not be), and the input must arrive intact (a close that
    // truncated the queued write would lose it).
    #[test]
    fn close_input_gives_a_reading_child_eof_after_its_input() {
        let mut jobs = JobManager::new().unwrap();
        jobs.start(7, collected("cat")).unwrap();
        assert!(jobs.send(7, b"123".to_vec()).unwrap());
        assert!(jobs.close_input(7));
        let (status, _) = jobs.wait(&[7], 5_000).unwrap();
        assert_eq!(status, vec![0], "child did not exit: stdin was left open");
        let (stdout, _) = jobs.take_buffered_output(7).unwrap();
        assert_eq!(stdout, b"123", "queued input was truncated by the close");
    }

    // A large input cannot fit one pipe buffer, so the synchronous write leaves
    // a remainder queued. The close has to pump the loop until it drains rather
    // than discard it.
    #[test]
    fn close_input_flushes_a_write_larger_than_one_pipe_buffer() {
        let payload = vec![b'x'; 512 * 1024];
        let mut jobs = JobManager::new().unwrap();
        jobs.start(8, collected("cat")).unwrap();
        assert!(jobs.send(8, payload.clone()).unwrap());
        assert!(jobs.close_input(8));
        let (status, _) = jobs.wait(&[8], 20_000).unwrap();
        assert_eq!(status, vec![0]);
        let (stdout, _) = jobs.take_buffered_output(8).unwrap();
        assert_eq!(
            stdout.len(),
            payload.len(),
            "the queued remainder was dropped"
        );
    }

    // Dropping the manager terminates a child that is still running, so no
    // child outlives the parent that was waiting on it. A detached child is the
    // one exception upstream makes.
    #[test]
    fn dropping_the_manager_terminates_a_live_child_but_not_a_detached_one() {
        let live_pid;
        let detached_pid;
        {
            let mut jobs = JobManager::new().unwrap();
            live_pid = jobs.start(9, options("sleep 60", true)).unwrap();
            let mut detached = options("sleep 60", true);
            detached.detached = true;
            detached_pid = jobs.start(10, detached).unwrap();
            // One turn so both children are really running before the drop.
            jobs.wait(&[9, 10], 200).unwrap();
        }
        assert!(
            !process_is_alive(live_pid),
            "a live child outlived its manager"
        );
        assert!(
            process_is_alive(detached_pid),
            "a detached child was terminated"
        );
        let _ignored = ox_uv::process::kill(detached_pid, Some(9));
    }

    fn process_is_alive(pid: u32) -> bool {
        // A terminated child is reaped by the manager's waiter, so the pid is
        // gone rather than a zombie. Retry briefly: SIGTERM delivery and the
        // reap are not instantaneous.
        for _ in 0..50 {
            if ox_uv::process::kill(pid, Some(0)).is_err() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    #[test]
    fn stdin_write_reaches_the_child() {
        let mut jobs = JobManager::new().unwrap();
        jobs.start(3, options("grep -qx hello", false)).unwrap();
        assert!(jobs.send(3, b"hello\n".to_vec()).unwrap());
        let (status, _) = jobs.wait(&[3], 2_000).unwrap();
        assert_eq!(status, vec![0]);
    }

    // `spawn_pty` must open the pty at the caller's geometry, not a fixed
    // 80x24: `f_jobstart` sizes a `term` job's pty from `curwin`
    // (eval/funcs.c:3505-3506) and `channel_job_start` keeps the
    // `pty_proc_init` default only for a dimension left at zero
    // (channel.c:394-398). `get_size` reads the kernel winsize back, so
    // this asserts the size the child was actually born with.
    #[test]
    fn pty_spawn_uses_the_requested_size() {
        let mut jobs = JobManager::new().unwrap();
        let mut pty = options("cat", true);
        pty.pty = true;
        pty.pty_size = PtySize {
            columns: 100,
            rows: 40,
        };
        jobs.start(3, pty).unwrap();
        let size = match jobs.jobs.get(&3).and_then(|job| job.input.as_ref()) {
            Some(JobInput::Pty(master)) => master.get_size().unwrap(),
            _ => panic!("pty job has no pty input"),
        };
        assert_eq!(
            size,
            PtySize {
                columns: 100,
                rows: 40
            }
        );
    }

    #[test]
    fn timeout_invalid_ids_and_sigterm_status_match_jobwait_contract() {
        let mut jobs = JobManager::new().unwrap();
        jobs.start(3, options("sleep 10", false)).unwrap();
        let (status, _) = jobs.wait(&[3, 999], 5).unwrap();
        assert_eq!(status, vec![-1, -3]);
        assert!(!jobs.send(999, Vec::new()).unwrap());
        assert!(!jobs.stop(999).unwrap());
        assert!(jobs.stop(3).unwrap());
        let (status, _) = jobs.wait(&[3], 2_000).unwrap();
        assert_eq!(status, vec![143]);
    }

    #[test]
    fn cwd_and_environment_are_applied_to_the_spawn() {
        // `pwd` reports the resolved directory — `/tmp` is a `/private/tmp`
        // symlink on macOS — so the grep pattern takes the canonical path.
        let tmp = std::fs::canonicalize("/tmp").unwrap();
        let mut job_options = options(
            &format!(
                "printenv OX_JOB_VALUE | grep -qx set && pwd | grep -qx {}",
                tmp.display()
            ),
            false,
        );
        let mut environment = std::env::vars_os().collect::<Vec<_>>();
        environment.push((OsString::from("OX_JOB_VALUE"), OsString::from("set")));
        job_options.environment = Some(environment);
        job_options.cwd = Some(PathBuf::from("/tmp"));
        let mut jobs = JobManager::new().unwrap();
        jobs.start(3, job_options).unwrap();
        let (status, _) = jobs.wait(&[3], 2_000).unwrap();
        assert_eq!(status, vec![0]);
    }

    // If a caller polls the reactor but has no callback host available, the
    // manager must keep those events and return them on the next poll so
    // `chansend`/`take_pty_output` paths never drop an `on_stdout`/`on_exit`.
    #[test]
    fn deferred_events_are_redelivered_by_the_next_poll() {
        let mut jobs = JobManager::new().unwrap();
        let options = Typval::dict(Vec::new());
        let Typval::Dict(reference) = options else {
            unreachable!()
        };
        let event = JobEvent {
            callback: Typval::String(OxStr::from("Callback")),
            receiver: reference,
            args: vec![
                Typval::Number(1),
                Typval::list(vec![Typval::String(OxStr::from("data"))]),
                Typval::String(OxStr::from("stdout")),
            ],
        };
        jobs.defer_events(vec![event.clone(), event.clone()]);
        let first = jobs.poll().unwrap();
        assert_eq!(
            first.len(),
            2,
            "deferred events must be returned on the next poll"
        );
        assert!(
            jobs.poll().unwrap().is_empty(),
            "deferred events must be drained after one poll"
        );
    }

    // A fire-and-forget job's exit callback must survive a poll path with no
    // host: the tick re-defers what it drains, and the drained batch reaches
    // `take_deferred_and_invoke` exactly once, in arrival order — upstream
    // delivers job callbacks on the main loop (`process_events` →
    // `channel_write` → `invoke_callback`).
    #[test]
    fn deferred_exit_event_is_delivered_exactly_once_through_the_invoke_host() {
        let mut jobs = JobManager::new().unwrap();
        jobs.start(3, options("true", true)).unwrap();
        // The tick shape: poll without a host, re-defer, repeat until the
        // exit event surfaces. A blocking wait() would consume the event
        // itself, which is the path this must not take.
        let mut events = Vec::new();
        for _ in 0..500 {
            let polled = jobs.poll().unwrap();
            let exited = polled.iter().any(|event| event_name(event) == "exit");
            events.extend(polled);
            if exited {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            events.iter().any(|event| event_name(event) == "exit"),
            "a finished job must queue its exit event"
        );
        jobs.defer_events(events);
        // The invoke host consumes the drained batch and records it.
        let mut invoked = Vec::new();
        jobs.take_deferred_and_invoke(|events| {
            let count = events.len();
            invoked.extend(events.drain(..count));
            Ok(())
        })
        .unwrap();
        let exits = invoked
            .iter()
            .filter(|event| event_name(event) == "exit")
            .count();
        assert_eq!(exits, 1, "on_exit must run exactly once");
        let status = invoked
            .iter()
            .find(|event| event_name(event) == "exit")
            .and_then(|event| match &event.args[1] {
                Typval::Number(status) => Some(*status),
                _ => None,
            });
        assert_eq!(status, Some(0), "on_exit must carry the exit code");
        // The queue is empty afterwards; a second poll is a no-op.
        let second = jobs.poll().unwrap();
        assert!(
            !second.iter().any(|event| event_name(event) == "exit"),
            "the exit event must not survive its invocation"
        );
    }

    // A handler that fails mid-batch must not destroy the uninvoked tail:
    // `invoke_job_events` drains from the front as it delivers, and
    // `take_deferred_and_invoke` requeues whatever remains (upstream keeps
    // unprocessed callbacks queued, `channel.c`).
    #[test]
    fn failed_handler_keeps_the_uninvoked_tail_deliverable() {
        let mut jobs = JobManager::new().unwrap();
        let options = Typval::dict(Vec::new());
        let Typval::Dict(reference) = options else {
            unreachable!()
        };
        let event = |name: &str| JobEvent {
            callback: Typval::String(OxStr::from("Callback")),
            receiver: reference.clone(),
            args: vec![
                Typval::Number(1),
                Typval::list(Vec::new()),
                Typval::String(OxStr::from(name)),
            ],
        };
        jobs.defer_events(vec![event("stdout"), event("exit")]);
        let result = jobs.take_deferred_and_invoke(|events| {
            // Deliver the first event, then fail: the exit event stays.
            let _ = events.remove(0);
            Err("handler failed".to_owned())
        });
        assert_eq!(result.unwrap_err(), "handler failed");
        let requeued = jobs.poll().unwrap();
        assert_eq!(
            requeued.len(),
            1,
            "the uninvoked tail must survive the failure"
        );
        assert_eq!(event_name(&requeued[0]), "exit");
    }
}
