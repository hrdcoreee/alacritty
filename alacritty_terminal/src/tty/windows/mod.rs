use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Result, Write};
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::TryRecvError;

use windows_sys::Win32::Foundation::HANDLE;

use crate::event::{OnResize, WindowSize};
use crate::tty::windows::child::ChildExitWatcher;
use crate::tty::{ChildEvent, EventedPty, EventedReadWrite, Options, Shell};

mod blocking;
mod child;
mod conpty;

use blocking::{UnblockedReader, UnblockedWriter};
use conpty::Conpty;
use miow::pipe::{AnonRead, AnonWrite};
use polling::{Event, Poller};

pub const PTY_CHILD_EVENT_TOKEN: usize = 1;
pub const PTY_READ_WRITE_TOKEN: usize = 2;

/// Capacity of the internal pipes used to decouple blocking PTY I/O from the event loop.
const PIPE_CAPACITY: usize = crate::event_loop::READ_BUFFER_SIZE;

/// Signal used to resize a pseudoconsole through the signal pipe.
///
/// This matches `PTY_SIGNAL_RESIZE_WINDOW` from Windows Terminal's `winconpty` implementation.
const PTY_SIGNAL_RESIZE_WINDOW: u16 = 8;

/// Handles received from the Windows default terminal handoff.
///
/// All handles are owned by this structure. They are duplicated into the Alacritty process by the
/// COM proxy stub before being handed over.
pub struct HandoffHandles {
    /// Pipe end used to write input to the shell.
    pub input: File,

    /// Pipe end used to read output from the shell.
    pub output: File,

    /// Pipe used to send resize events to the console host.
    pub signal: File,

    /// Reference handle that keeps the pseudoconsole alive in the console host.
    pub reference: Mutex<Option<OwnedHandle>>,

    /// Handle to the console host process.
    pub server: OwnedHandle,

    /// Handle to the shell process.
    pub client: OwnedHandle,

    /// Initial window title provided by the console host.
    pub title: Option<String>,
}

impl HandoffHandles {
    /// Take the pseudoconsole reference handle so it can be closed once the PTY is set up.
    fn take_reference(&self) -> Option<OwnedHandle> {
        self.reference.lock().ok().and_then(|mut reference| reference.take())
    }
}

/// A cloneable reference to the handles of a handed off terminal.
///
/// The handles are shared to avoid duplicating them whenever the window options are cloned.
#[derive(Clone)]
pub struct Handoff(Arc<HandoffHandles>);

impl Handoff {
    pub fn new(handles: HandoffHandles) -> Self {
        Self(Arc::new(handles))
    }

    /// Initial window title provided by the console host.
    pub fn title(&self) -> Option<&str> {
        self.0.title.as_deref()
    }

    fn handles(&self) -> &HandoffHandles {
        &self.0
    }
}

impl fmt::Debug for Handoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handoff").finish_non_exhaustive()
    }
}

impl PartialEq for Handoff {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Handoff {}

/// Source of the terminal output stream.
pub enum ReadSource {
    /// Pipe created by Alacritty.
    Anon(AnonRead),
    /// Pipe received through the terminal handoff.
    File(File),
}

impl Read for ReadSource {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self {
            Self::Anon(reader) => reader.read(buf),
            Self::File(file) => file.read(buf),
        }
    }
}

/// Destination of the terminal input stream.
pub enum WriteSink {
    /// Pipe created by Alacritty.
    Anon(AnonWrite),
    /// Pipe received through the terminal handoff.
    File(File),
}

impl Write for WriteSink {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        match self {
            Self::Anon(writer) => writer.write(buf),
            Self::File(file) => file.write(buf),
        }
    }

    fn flush(&mut self) -> Result<()> {
        match self {
            Self::Anon(writer) => writer.flush(),
            Self::File(file) => file.flush(),
        }
    }
}

type ReadPipe = UnblockedReader<ReadSource>;
type WritePipe = UnblockedWriter<WriteSink>;

/// Pseudoconsole backend used by the PTY.
enum Backend {
    /// Pseudoconsole spawned by Alacritty.
    Conpty(Conpty),
    /// Pseudoconsole handed off by the Windows console host.
    Handoff(HandoffPty),
}

impl OnResize for Backend {
    fn on_resize(&mut self, window_size: WindowSize) {
        match self {
            Self::Conpty(conpty) => conpty.on_resize(window_size),
            Self::Handoff(handoff) => handoff.on_resize(window_size),
        }
    }
}

/// Pseudoconsole handed off by the Windows console host.
struct HandoffPty {
    signal: File,
}

impl HandoffPty {
    fn new(signal: File) -> Self {
        Self { signal }
    }
}

impl OnResize for HandoffPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        // The console host expects the dimensions as 16-bit little-endian integers.
        let columns = window_size.num_cols;
        let lines = window_size.num_lines;

        let packet = [
            PTY_SIGNAL_RESIZE_WINDOW as u8,
            (PTY_SIGNAL_RESIZE_WINDOW >> 8) as u8,
            columns as u8,
            (columns >> 8) as u8,
            lines as u8,
            (lines >> 8) as u8,
        ];

        let mut signal = &self.signal;
        if let Err(err) = signal.write_all(&packet) {
            log::warn!("Failed to resize handed off terminal: {err}");
        }
    }
}

/// Process handles that must outlive the [`ChildExitWatcher`].
struct HandoffProcess {
    _server: OwnedHandle,
    _client: OwnedHandle,
    _reference: Option<OwnedHandle>,
}

pub struct Pty {
    // XXX: Backend is required to be the first field, to ensure correct drop order. Dropping
    // `conout` before `backend` will cause a deadlock (with Conpty).
    backend: Backend,
    conout: ReadPipe,
    conin: WritePipe,
    child_watcher: ChildExitWatcher,
    // Keep the process handles alive until after the `child_watcher` is dropped.
    _handoff: Option<HandoffProcess>,
}

pub fn new(config: &Options, window_size: WindowSize, _window_id: u64) -> Result<Pty> {
    conpty::new(config, window_size)
}

/// Create a PTY from handles received through the Windows default terminal handoff.
pub fn new_handoff(_config: &Options, window_size: WindowSize, handoff: &Handoff) -> Result<Pty> {
    let handles = handoff.handles();

    // Duplicate the handles we keep, since `Handoff` may outlive the PTY setup.
    let input = handles.input.try_clone()?;
    let output = handles.output.try_clone()?;
    let signal = handles.signal.try_clone()?;

    let client = handles.client.try_clone()?;
    let child_watcher = ChildExitWatcher::new(client.as_raw_handle() as HANDLE)?;

    let mut backend = Backend::Handoff(HandoffPty::new(signal));

    // Tell the console host about our initial size before any output can be written.
    backend.on_resize(window_size);

    let conin = UnblockedWriter::new(WriteSink::File(input), PIPE_CAPACITY);
    let conout = UnblockedReader::new(ReadSource::File(output), PIPE_CAPACITY);

    // Now that the terminal is fully set up, release the pseudoconsole reference, so the console
    // host can exit once all clients have disconnected.
    let reference = handles.take_reference();

    let process = HandoffProcess { _server: handles.server.try_clone()?, _client: client, _reference: reference };

    log::info!("Attached to handed off pseudoconsole, child PID: {:?}", child_watcher.pid());

    Ok(Pty { backend, conout, conin, child_watcher, _handoff: Some(process) })
}

impl Pty {
    /// Create a PTY from handles received through the Windows default terminal handoff.
    pub fn from_handoff_handles(window_size: WindowSize, handoff: &Handoff) -> Result<Pty> {
        new_handoff(&Options::default(), window_size, handoff)
    }
    fn new(
        backend: Backend,
        conout: ReadPipe,
        conin: WritePipe,
        child_watcher: ChildExitWatcher,
        handoff: Option<HandoffProcess>,
    ) -> Self {
        Self { backend, conout, conin, child_watcher, _handoff: handoff }
    }

    pub fn child_watcher(&self) -> &ChildExitWatcher {
        &self.child_watcher
    }
}

fn with_key(mut event: Event, key: usize) -> Event {
    event.key = key;
    event
}

impl EventedReadWrite for Pty {
    type Reader = ReadPipe;
    type Writer = WritePipe;

    #[inline]
    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        interest: polling::Event,
        poll_opts: polling::PollMode,
    ) -> io::Result<()> {
        self.conin.register(poll, with_key(interest, PTY_READ_WRITE_TOKEN), poll_opts);
        self.conout.register(poll, with_key(interest, PTY_READ_WRITE_TOKEN), poll_opts);
        self.child_watcher.register(poll, with_key(interest, PTY_CHILD_EVENT_TOKEN));

        Ok(())
    }

    #[inline]
    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        interest: polling::Event,
        poll_opts: polling::PollMode,
    ) -> io::Result<()> {
        self.conin.register(poll, with_key(interest, PTY_READ_WRITE_TOKEN), poll_opts);
        self.conout.register(poll, with_key(interest, PTY_READ_WRITE_TOKEN), poll_opts);
        self.child_watcher.register(poll, with_key(interest, PTY_CHILD_EVENT_TOKEN));

        Ok(())
    }

    #[inline]
    fn deregister(&mut self, _poll: &Arc<Poller>) -> io::Result<()> {
        self.conin.deregister();
        self.conout.deregister();
        self.child_watcher.deregister();

        Ok(())
    }

    #[inline]
    fn reader(&mut self) -> &mut Self::Reader {
        &mut self.conout
    }

    #[inline]
    fn writer(&mut self) -> &mut Self::Writer {
        &mut self.conin
    }
}

impl EventedPty for Pty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        match self.child_watcher.event_rx().try_recv() {
            Ok(ev) => Some(ev),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(ChildEvent::Exited(None)),
        }
    }
}

impl OnResize for Pty {
    fn on_resize(&mut self, window_size: WindowSize) {
        self.backend.on_resize(window_size)
    }
}

// Modified per stdlib implementation.
// https://github.com/rust-lang/rust/blob/6707bf0f59485cf054ac1095725df43220e4be20/library/std/src/sys/args/windows.rs#L174
fn push_escaped_arg(cmd: &mut String, arg: &str) {
    let arg_bytes = arg.as_bytes();
    let quote = arg_bytes.iter().any(|c| *c == b' ' || *c == b'\t') || arg_bytes.is_empty();
    if quote {
        cmd.push('"');
    }

    let mut backslashes: usize = 0;
    for x in arg.chars() {
        if x == '\\' {
            backslashes += 1;
        } else {
            if x == '"' {
                // Add n+1 backslashes to total 2n+1 before internal '"'.
                cmd.extend((0..=backslashes).map(|_| '\\'));
            }
            backslashes = 0;
        }
        cmd.push(x);
    }

    if quote {
        // Add n backslashes to total 2n before ending '"'.
        cmd.extend((0..backslashes).map(|_| '\\'));
        cmd.push('"');
    }
}

fn cmdline(config: &Options) -> String {
    let default_shell = Shell::new("powershell".to_owned(), Vec::new());
    let shell = config.shell.as_ref().unwrap_or(&default_shell);

    let mut cmd = String::new();
    cmd.push_str(&shell.program);

    for arg in &shell.args {
        cmd.push(' ');
        if config.escape_args {
            push_escaped_arg(&mut cmd, arg);
        } else {
            cmd.push_str(arg)
        }
    }
    cmd
}

/// Converts the string slice into a Windows-standard representation for "W"-
/// suffixed function variants, which accept UTF-16 encoded string values.
pub fn win32_string<S: AsRef<OsStr> + ?Sized>(value: &S) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(once(0)).collect()
}

#[cfg(test)]
mod test {
    use crate::tty::windows::{cmdline, push_escaped_arg};
    use crate::tty::{Options, Shell};

    #[test]
    fn test_escape() {
        let test_set = vec![
            // Basic cases - no escaping needed
            ("abc", "abc"),
            // Cases requiring quotes (space/tab)
            ("", "\"\""),
            (" ", "\" \""),
            ("ab c", "\"ab c\""),
            ("ab\tc", "\"ab\tc\""),
            // Cases with backslashes only (no spaces, no quotes) - no quotes added
            ("ab\\c", "ab\\c"),
            // Cases with quotes only (no spaces) - quotes escaped but no outer quotes
            ("ab\"c", "ab\\\"c"),
            ("\"", "\\\""),
            ("a\"b\"c", "a\\\"b\\\"c"),
            // Cases requiring both quotes and escaping (contains spaces)
            ("ab \"c", "\"ab \\\"c\""),
            ("a \"b\" c", "\"a \\\"b\\\" c\""),
            // Complex real-world cases
            ("C:\\Program Files\\", "\"C:\\Program Files\\\\\""),
            ("C:\\Program Files\\a.txt", "\"C:\\Program Files\\a.txt\""),
            (
                r#"sh -c "cd /home/user; ARG='abc' \""'${SHELL:-sh}" -i -c '"'echo hello'""#,
                r#""sh -c \"cd /home/user; ARG='abc' \\\"\"'${SHELL:-sh}\" -i -c '\"'echo hello'\"""#,
            ),
        ];

        for (input, expected) in test_set {
            let mut escaped_arg = String::new();
            push_escaped_arg(&mut escaped_arg, input);
            assert_eq!(escaped_arg, expected, "Failed for input: {}", input);
        }
    }

    #[test]
    fn test_cmdline() {
        let mut options = Options {
            shell: Some(Shell {
                program: "echo".to_string(),
                args: vec!["hello world".to_string()],
            }),
            working_directory: None,
            drain_on_exit: true,
            env: Default::default(),
            escape_args: false,
        };
        assert_eq!(cmdline(&options), "echo hello world");

        options.escape_args = true;
        assert_eq!(cmdline(&options), "echo \"hello world\"");
    }
}
