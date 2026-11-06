//! Public, current-user/session-only recorder protocol. No capture runs on this thread.

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Security::Authorization::*;
use windows::Win32::Security::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Pipes::*;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

pub const VERSION: u32 = 1;
pub const WM_CONTROL: u32 = WM_APP + 60;
const LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Capture,
    Record {
        #[serde(default)]
        fullscreen: bool,
        system_sound: Option<bool>,
        mic: Option<bool>,
    },
    Stop,
    Pause,
    Resume,
    Status,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Request {
    pub version: u32,
    #[serde(flatten)]
    pub command: Command,
}

fn decode(bytes: &[u8]) -> std::result::Result<Request, String> {
    // Serde's tagged unit variants otherwise ignore extra fields. Enforce the
    // same recording-only options as the CLI before dispatching anything.
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let fields = value.as_object().ok_or("Request must be an object")?;
    let recording = fields.get("command").and_then(|v| v.as_str()) == Some("record");
    if fields.keys().any(|key| {
        !matches!(key.as_str(), "version" | "command")
            && !(recording && matches!(key.as_str(), "fullscreen" | "system_sound" | "mic"))
    }) {
        return Err("Unknown or inapplicable command option".into());
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

/// Launch options for the tray host plus the optional command to run once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Options {
    pub command: Option<Command>,
    /// Overrides config.toml's theme_file for this host.
    pub theme_file: Option<String>,
    /// Leaves PrintScreen and Win+Shift+S/R to the host shell.
    pub no_hotkeys: bool,
}

/// Legacy --capture remains valid. Controls other than record/capture never start a host.
pub fn parse(args: impl IntoIterator<Item = String>) -> std::result::Result<Options, String> {
    let mut options = Options::default();
    let mut fullscreen = false;
    let mut sound = None;
    let mut mic = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let next = match arg.as_str() {
            "--theme-file" => {
                let path = args
                    .next()
                    .filter(|path| !path.is_empty() && !path.starts_with("--"))
                    .ok_or("--theme-file requires a path")?;
                options.theme_file = Some(path);
                None
            }
            "--no-hotkeys" => {
                options.no_hotkeys = true;
                None
            }
            "--capture" => Some(Command::Capture),
            "--record" => Some(Command::Record { fullscreen: false, system_sound: None, mic: None }),
            "--stop" => Some(Command::Stop),
            "--pause" => Some(Command::Pause),
            "--resume" => Some(Command::Resume),
            "--status" => Some(Command::Status),
            "--fullscreen" => {
                fullscreen = true;
                None
            }
            "--system-sound" | "--mic" => {
                let value = match args.next().as_deref() {
                    Some("on") => true,
                    Some("off") => false,
                    _ => return Err(format!("{arg} requires on or off")),
                };
                if arg == "--mic" {
                    mic = Some(value);
                } else {
                    sound = Some(value);
                }
                None
            }
            _ => return Err(format!("Unknown argument: {arg}")),
        };
        if let Some(next) = next {
            if options.command.replace(next).is_some() {
                return Err("Use exactly one command".into());
            }
        }
    }
    if matches!(options.command, Some(Command::Record { .. })) {
        options.command = Some(Command::Record { fullscreen, system_sound: sound, mic });
    } else if fullscreen || sound.is_some() || mic.is_some() {
        return Err("Recording options require --record".into());
    }
    Ok(options)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Selecting,
    Ready,
    Recording,
    Paused,
    Finalizing,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Event {
    pub sequence: u64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct State {
    pub state: Phase,
    pub elapsed_ms: u64,
    pub system_sound: bool,
    pub mic: bool,
    pub events: Vec<Event>,
    pub event_sequence: u64,
    pub output_directory: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            state: Phase::Idle,
            elapsed_ms: 0,
            system_sound: true,
            mic: false,
            events: Vec::new(),
            event_sequence: 0,
            output_directory: String::new(),
        }
    }
}

impl State {
    fn change(&mut self, phase: Phase, event: Option<(&str, Option<String>, Option<String>)>) {
        self.state = phase;
        if let Some((kind, output_path, message)) = event {
            self.event_sequence += 1;
            self.events.push(Event { sequence: self.event_sequence, kind: kind.into(), output_path, message });
            if self.events.len() > 32 {
                self.events.remove(0);
            }
        }
    }
}

thread_local! { static STATE: RefCell<State> = RefCell::new(State::default()); }

pub fn phase() -> Phase {
    STATE.with_borrow(|s| s.state)
}

pub fn change(phase: Phase, event: Option<&str>, path: Option<String>, message: Option<String>) {
    STATE.with_borrow_mut(|s| {
        if phase == Phase::Selecting || phase == Phase::Ready && s.state == Phase::Idle {
            s.elapsed_ms = 0;
        }
        s.change(phase, event.map(|event| (event, path, message)));
    });
}

pub fn error(message: String) {
    change(phase(), Some("error"), None, Some(message));
}

/// This guard is shared by the real dispatch path and unit tests.
pub fn accepts(phase: Phase, command: &Command) -> bool {
    match command {
        Command::Record { .. } => phase == Phase::Idle,
        Command::Pause => phase == Phase::Recording,
        Command::Resume => phase == Phase::Paused,
        // Stop is always safe and idempotent. In idle it does absolutely nothing.
        Command::Stop | Command::Status | Command::Capture => true,
    }
}

#[derive(Serialize)]
pub struct Response {
    version: u32,
    ok: bool,
    host_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten)]
    state: State,
}

static HOST_ID: OnceLock<String> = OnceLock::new();

pub fn response(result: std::result::Result<(), String>) -> Response {
    crate::recording::update_status();
    Response {
        version: VERSION,
        ok: result.is_ok(),
        host_id: HOST_ID.get_or_init(|| format!("{}-{:?}", std::process::id(), std::time::SystemTime::now())).clone(),
        error: result.err(),
        state: STATE.with_borrow(Clone::clone),
    }
}

pub fn update(elapsed_ms: u64, audio: crate::recording::AudioState) {
    STATE.with_borrow_mut(|s| {
        s.elapsed_ms = elapsed_ms;
        s.system_sound = audio.sound;
        s.mic = audio.mic;
    });
}

pub fn directory(cfg: &crate::config::Config) {
    if let Ok(path) = crate::recording_media::directory(cfg) {
        STATE.with_borrow_mut(|s| s.output_directory = path.to_string_lossy().into_owned());
    }
}

type Envelope = (Command, mpsc::Sender<Response>);
static INBOX: OnceLock<Mutex<mpsc::Receiver<Envelope>>> = OnceLock::new();

pub fn dispatch() {
    if let Some(inbox) = INBOX.get() {
        loop {
            // Commands may pump COM/window messages. Release the inbox lock
            // before dispatch, which may re-enter this function.
            let request = { inbox.lock().unwrap().try_recv() };
            let Ok((command, reply)) = request else { break };
            let result = crate::host::control(command);
            if let Err(e) = &result {
                error(e.clone());
            }
            let _ = reply.send(response(result));
        }
    }
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Local(*mut std::ffi::c_void);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0)));
        }
    }
}

fn identity() -> Result<(String, String)> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let token = Handle(token);
        let mut size = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut size);
        let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        GetTokenInformation(token.0, TokenUser, Some(buffer.as_mut_ptr().cast()), size, &mut size)?;
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid)?;
        let _sid = Local(sid.0.cast());
        let sid = sid.to_string()?;
        let mut session = 0;
        ProcessIdToSessionId(GetCurrentProcessId(), &mut session)?;
        let name = format!(r"\\.\pipe\Glazier.Control.v1.{sid}.{session}");
        // Unit tests must never connect to or compete with the installed host.
        #[cfg(test)]
        let name = format!("{name}.test-{}", std::process::id());
        Ok((name, sid))
    }
}

/// The pipe is created synchronously before any UI so startup failures are visible.
pub fn start(hwnd: HWND) -> Result<()> {
    let (name, sid) = identity()?;
    let pipe = unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(format!("D:P(A;;GA;;;{sid})")),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
        let _descriptor = Local(descriptor.0);
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: FALSE,
        };
        let handle = CreateNamedPipeW(
            &HSTRING::from(name),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            LIMIT as u32,
            LIMIT as u32,
            0,
            Some(&security),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(Error::from_thread());
        }
        handle.0 as isize
    };
    let (send, receive) = mpsc::channel();
    let _ = INBOX.set(Mutex::new(receive));
    let hwnd = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("Glazier control".into())
        .spawn(move || {
            let handle = HANDLE(pipe as *mut _);
            let _handle = Handle(handle);
            loop {
                unsafe {
                    let connected = ConnectNamedPipe(handle, None);
                    if connected.is_err() && GetLastError() == ERROR_NO_DATA {
                        // A client opened and closed between polls. Reset the instance or it stays busy.
                        let _ = DisconnectNamedPipe(handle);
                        continue;
                    }
                    if connected.is_err() && GetLastError() != ERROR_PIPE_CONNECTED {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    let result = serve(handle, &send, HWND(hwnd as *mut _));
                    if let Err(e) = result {
                        // Invalid requests get a machine-readable error, never a tray popup.
                        let body = serde_json::json!({"version": VERSION, "ok": false, "error": e});
                        let _ = write_response(handle, body.to_string().into_bytes());
                    }
                    let _ = DisconnectNamedPipe(handle);
                }
            }
        })
        .map_err(|e| Error::new(E_FAIL, e.to_string()))?;
    Ok(())
}

unsafe fn serve(pipe: HANDLE, send: &mpsc::Sender<Envelope>, hwnd: HWND) -> std::result::Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 2048];
        let mut count = 0;
        let result = unsafe { ReadFile(pipe, Some(&mut chunk), Some(&mut count), None) };
        bytes.extend_from_slice(&chunk[..count as usize]);
        if bytes.len() > LIMIT {
            return Err("Request too large".into());
        }
        if bytes.contains(&b'\n') {
            break;
        }
        if result.is_err() && unsafe { GetLastError() } != ERROR_NO_DATA {
            return Err("Client disconnected".into());
        }
        if Instant::now() > deadline {
            return Err("Request timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let request = decode(&bytes)?;
    if request.version != VERSION {
        return Err("Unsupported protocol version".into());
    }
    let (reply, receive) = mpsc::channel();
    send.send((request.command, reply)).map_err(|e| e.to_string())?;
    unsafe { PostMessageW(Some(hwnd), WM_CONTROL, WPARAM(0), LPARAM(0)) }.map_err(|e| e.to_string())?;
    let response = receive.recv_timeout(Duration::from_secs(10)).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec(&response).map_err(|e| e.to_string())?;
    unsafe { write_response(pipe, bytes) }
}

unsafe fn write_response(pipe: HANDLE, mut bytes: Vec<u8>) -> std::result::Result<(), String> {
    if bytes.len() >= LIMIT {
        return Err("Response too large".into());
    }
    bytes.push(b'\n');
    unsafe { WriteFile(pipe, Some(&bytes), None, None) }.map_err(|e| e.to_string())?;
    // DisconnectNamedPipe discards unread bytes. Wait for the client to consume
    // the response and acknowledge it, but never block the server on a peer.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let mut ack = [0u8; 1];
        let mut count = 0;
        let result = unsafe { ReadFile(pipe, Some(&mut ack), Some(&mut count), None) };
        if count > 0 || result.is_err() && unsafe { GetLastError() } != ERROR_NO_DATA {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn client(command: &Command) -> std::result::Result<serde_json::Value, String> {
    exchange(&Request { version: VERSION, command: command.clone() })
}

fn exchange(request: &Request) -> std::result::Result<serde_json::Value, String> {
    let (name, _) = identity().map_err(|e| e.to_string())?;
    // Do not let a pipe server impersonate the caller, and do not inherit handles.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut pipe = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(SECURITY_SQOS_PRESENT.0 | SECURITY_ANONYMOUS.0)
            .open(&name)
        {
            Ok(pipe) => break pipe,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(_) => return Err("Glazier control v1 is not available (not running, old version, or busy)".into()),
        }
    };
    let mut bytes = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    pipe.write_all(&bytes).map_err(|e| format!("Writing request: {e}"))?;
    // Poll bytes before reading. Changing a client's wait mode can race
    // ConnectNamedPipe and fail with ERROR_PIPE_BUSY.
    use std::os::windows::io::AsRawHandle;
    let deadline = Instant::now() + Duration::from_secs(12);
    bytes.clear();
    loop {
        let mut available = 0;
        unsafe { PeekNamedPipe(HANDLE(pipe.as_raw_handle()), None, 0, None, Some(&mut available), None) }
            .map_err(|e| format!("Reading pipe status: {e}"))?;
        let mut chunk = [0u8; 2048];
        if available > 0 {
            let n = pipe
                .read(&mut chunk[..(available as usize).min(2048)])
                .map_err(|e| format!("Reading response: {e}"))?;
            if n == 0 {
                return Err("Glazier disconnected without a response".into());
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
        if bytes.len() > LIMIT {
            return Err("Response too large".into());
        }
        if bytes.contains(&b'\n') {
            let response = serde_json::from_slice(&bytes).map_err(|e| e.to_string());
            let _ = pipe.write_all(b"\n");
            return response;
        }
        if Instant::now() > deadline {
            return Err("Glazier control timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn print(result: std::result::Result<serde_json::Value, String>) -> i32 {
    let value = result.unwrap_or_else(|e| serde_json::json!({"version": VERSION, "ok": false, "error": e}));
    let _ = writeln!(std::io::stdout(), "{value}");
    if value["ok"] == true { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(s: &str) -> std::result::Result<Options, String> {
        parse(s.split_whitespace().map(str::to_string))
    }

    fn command(s: &str) -> std::result::Result<Option<Command>, String> {
        args(s).map(|options| options.command)
    }

    #[test]
    fn parses_commands_and_audio() {
        assert_eq!(command("").unwrap(), None);
        for (s, expected) in [
            ("--capture", Command::Capture),
            ("--stop", Command::Stop),
            ("--pause", Command::Pause),
            ("--resume", Command::Resume),
            ("--status", Command::Status),
        ] {
            assert_eq!(command(s).unwrap(), Some(expected));
        }
        assert_eq!(
            command("--record --fullscreen --system-sound off --mic on").unwrap(),
            Some(Command::Record { fullscreen: true, system_sound: Some(false), mic: Some(true) })
        );
        for bad in ["--stop --record", "--status --mic on", "--record --mic", "--record --mic yes", "--unknown"] {
            assert!(args(bad).is_err(), "{bad}");
        }
        let request = Request { version: VERSION, command: command("--record").unwrap().unwrap() };
        let json = serde_json::to_vec(&request).unwrap();
        assert!(matches!(serde_json::from_slice::<Request>(&json).unwrap().command, Command::Record { .. }));
        assert!(decode(br#"{"version":1,"command":"record","unexpected":true}"#).is_err());
        assert!(decode(br#"{"version":1,"command":"stop","mic":true}"#).is_err());
    }

    #[test]
    fn parses_launch_options_before_or_after_the_command() {
        let options = args("--no-hotkeys --theme-file C:\\theme.json --capture").unwrap();
        assert_eq!(options.command, Some(Command::Capture));
        assert!(options.no_hotkeys);
        assert_eq!(options.theme_file.as_deref(), Some(r"C:\theme.json"));
        assert!(args("--capture --no-hotkeys").unwrap().no_hotkeys);
        assert!(args("--theme-file").is_err());
        assert!(args("--theme-file --capture").is_err());
    }

    #[test]
    fn transitions_and_finished_path_only_after_finalizing() {
        let mut s = State::default();
        for phase in
            [Phase::Selecting, Phase::Ready, Phase::Recording, Phase::Paused, Phase::Recording, Phase::Finalizing]
        {
            s.change(phase, None);
            assert_eq!(s.state, phase);
            assert!(s.events.iter().all(|e| e.output_path.is_none()));
        }
        s.change(Phase::Idle, Some(("finished", Some("final.mp4".into()), None)));
        assert_eq!(s.events[0].output_path.as_deref(), Some("final.mp4"));
        s.change(Phase::Idle, Some(("cancelled", None, None)));
        assert_eq!(s.event_sequence, 2);
    }

    #[test]
    fn stop_never_starts_and_pause_is_explicit() {
        for phase in [Phase::Idle, Phase::Selecting, Phase::Ready, Phase::Recording, Phase::Paused, Phase::Finalizing] {
            assert!(accepts(phase, &Command::Stop));
            assert_eq!(accepts(phase, &Command::Pause), phase == Phase::Recording);
            assert_eq!(accepts(phase, &Command::Resume), phase == Phase::Paused);
        }
        assert!(crate::host::control(Command::Stop).is_ok());
        assert_eq!(phase(), Phase::Idle);
        assert!(!crate::recording::exists());
    }

    #[test]
    fn private_pipe_round_trip_and_version_rejection_without_a_desktop_host() {
        unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            if msg == WM_CONTROL {
                dispatch();
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
            }
        }
        unsafe {
            let class = w!("GlazierControlUnitTest");
            RegisterClassW(&WNDCLASSW { lpszClassName: class, lpfnWndProc: Some(wndproc), ..Default::default() });
            let window = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
            .unwrap();
            start(window).unwrap();
            SetTimer(Some(window), 1, 15000, None);
            let address = window.0 as isize;
            let worker = std::thread::spawn(move || {
                let result = (|| -> std::result::Result<(), String> {
                    for _ in 0..4 {
                        let state = client(&Command::Status)?;
                        assert_eq!(state["version"], VERSION);
                        assert_eq!(state["state"], "idle");
                        assert_eq!(client(&Command::Stop)?["state"], "idle");
                    }
                    let bad = exchange(&Request { version: 999, command: Command::Status })?;
                    assert_eq!(bad["ok"], false);
                    assert_eq!(bad["error"], "Unsupported protocol version");
                    assert_eq!(client(&Command::Status)?["ok"], true);
                    Ok(())
                })();
                let _ = PostMessageW(Some(HWND(address as *mut _)), WM_APP + 61, WPARAM(0), LPARAM(0));
                result
            });
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, Some(window), 0, 0).0 > 0 {
                assert_ne!(msg.message, WM_TIMER, "private pipe test timed out");
                if msg.message == WM_APP + 61 {
                    break;
                }
                DispatchMessageW(&msg);
            }
            worker.join().unwrap().unwrap();
            let _ = KillTimer(Some(window), 1);
            DestroyWindow(window).unwrap();
        }
    }
}
