//! WSL ConPTY stdin truncation repro (Rust)
//!
//! Ports the relevant parts of Warp's Windows PTY stack:
//!   - pipes.rs       : bidirectional named pipe via NtCreateNamedPipeFile
//!   - conpty_api.rs  : loads conpty.dll, passes SAME handle for hInput+hOutput
//!   - windows/mod.rs : CreateProcessW with ConPTY attribute
//!   - event_loop.rs  : mio read+write loop with WouldBlock handling
//!
//! The heredoc sends 410 uniform 200-char lines of 'A's (~82 KB total).
//! bash writes the received content to a file; we read it back and compare
//! line count received vs. sent — showing both total bytes dropped and
//! approximate drop location, in a single run.
//!
//! Usage: wsl-stdin-repro [DISTRO_NAME]

#[cfg(windows)]
// All functions touching Win32 APIs are already marked `unsafe fn`;
// we don't need redundant `unsafe { }` blocks inside them.
#[allow(unsafe_op_in_unsafe_fn)]
mod repro {
    use std::ffi::OsString;
    use std::io::{self, Read, Write};
    use std::mem;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::FromRawHandle;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use mio::windows::NamedPipe;
    use mio::{Events, Interest, Poll, Token};

    use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_PIPE_BYTE_STREAM_MODE,
        FILE_PIPE_BYTE_STREAM_TYPE, FILE_PIPE_QUEUE_OPERATION, FILE_SYNCHRONOUS_IO_NONALERT,
        NTCREATEFILE_CREATE_OPTIONS, NtCreateFile,
    };
    use windows::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE,
        UNICODE_STRING,
    };
    use windows::Win32::Storage::FileSystem::{
        FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        SYNCHRONIZE,
    };
    use windows::Win32::System::Console::{COORD, HPCON};
    use windows::Win32::System::IO::IO_STATUS_BLOCK;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
        STARTUPINFOEXW, STARTUPINFOW, UpdateProcThreadAttribute, WaitForSingleObject,
    };
    use windows::Win32::System::WindowsProgramming::RtlInitUnicodeString;
    use windows::core::{HSTRING, PCWSTR};

    // ── Pipe creation (Warp's pipes.rs) ─────────────────────────────────────

    const BUFFER_SIZE: u32 = 128 * 1024;

    #[link(name = "ntdll.dll", kind = "raw-dylib", modifiers = "+verbatim")]
    unsafe extern "system" {
        fn NtCreateNamedPipeFile(
            FileHandle: *mut HANDLE,
            DesiredAccess: u32,
            ObjectAttributes: *mut OBJECT_ATTRIBUTES,
            IoStatusBlock: *mut IO_STATUS_BLOCK,
            ShareAccess: u32,
            CreateDisposition: u32,
            CreateOptions: u32,
            NamedPipeType: u32,
            ReadMode: u32,
            CompletionMode: u32,
            MaximumInstances: u32,
            InboundQuota: u32,
            OutboundQuota: u32,
            DefaultTimeout: *mut i64,
        ) -> NTSTATUS;
    }

    fn new_oa() -> OBJECT_ATTRIBUTES {
        OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            ..Default::default()
        }
    }

    /// Bidirectional pipe (Warp's pipes.rs).  Returns (client, server).
    /// client → passed to ConPTY as BOTH hInput and hOutput
    /// server → host reads output AND writes input via mio
    unsafe fn create_duplex_pipe() -> windows::core::Result<(HANDLE, HANDLE)> {
        let mut dev_path = UNICODE_STRING::default();
        RtlInitUnicodeString(&mut dev_path, windows::core::w!(r"\Device\NamedPipe\"));
        let mut oa = new_oa();
        oa.ObjectName = &dev_path;
        let mut iosb = IO_STATUS_BLOCK::default();
        let mut pipe_dir = HANDLE::default();
        windows::core::HRESULT::from(NtCreateFile(
            &mut pipe_dir,
            FILE_ACCESS_RIGHTS(GENERIC_READ.0 | SYNCHRONIZE.0),
            &oa,
            &mut iosb,
            None,
            FILE_FLAGS_AND_ATTRIBUTES::default(),
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
            FILE_SYNCHRONOUS_IO_NONALERT,
            None,
            0,
        ))
        .ok()?;

        let access = FILE_ACCESS_RIGHTS(GENERIC_READ.0 | GENERIC_WRITE.0 | SYNCHRONIZE.0);
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
        let mut timeout: i64 = -1_000_000_000;

        let empty = UNICODE_STRING::default();
        let mut oa2 = new_oa();
        oa2.ObjectName = &empty;
        oa2.Attributes = OBJ_CASE_INSENSITIVE;
        oa2.RootDirectory = pipe_dir;
        let mut iosb2 = IO_STATUS_BLOCK::default();

        let mut server = HANDLE::default();
        windows::core::HRESULT::from(NtCreateNamedPipeFile(
            &mut server,
            access.0,
            &mut oa2,
            &mut iosb2,
            share.0,
            FILE_CREATE.0,
            NTCREATEFILE_CREATE_OPTIONS::default().0, // async (overlapped)
            FILE_PIPE_BYTE_STREAM_TYPE,
            FILE_PIPE_BYTE_STREAM_MODE,
            FILE_PIPE_QUEUE_OPERATION,
            1,
            BUFFER_SIZE,
            BUFFER_SIZE,
            &mut timeout,
        ))
        .ok()?;

        let mut oa3 = new_oa();
        oa3.ObjectName = &empty;
        oa3.RootDirectory = server;
        oa3.Attributes = OBJ_CASE_INSENSITIVE;
        let mut iosb3 = IO_STATUS_BLOCK::default();
        let mut client = HANDLE::default();
        windows::core::HRESULT::from(NtCreateFile(
            &mut client,
            access,
            &mut oa3,
            &mut iosb3,
            None,
            FILE_FLAGS_AND_ATTRIBUTES::default(),
            share,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE,
            None,
            0,
        ))
        .ok()?;

        let _ = CloseHandle(pipe_dir);
        Ok((client, server))
    }

    // ── conpty.dll finder ──────────────────────────────────────────────────────────────────

    /// Search for conpty.dll, trying the following locations in order:
    ///
    /// 1. `CONPTY_DLL_PATH` env var — explicit override for any install location
    /// 2. Same directory as this binary — simplest: just `copy conpty.dll target\debug\`
    /// 3. Windows Terminal (Microsoft Store) — present on most developer machines
    /// 4. WezTerm — another common terminal that ships conpty.dll
    ///
    /// Note: we do NOT search Warp's copy.  Warp ships a privately-forked
    /// conpty.dll that may have different behaviour and would give false results.
    unsafe fn find_conpty_dll() -> Option<Conpty> {
        // 1. Explicit override
        if let Ok(path) = std::env::var("CONPTY_DLL_PATH") {
            if let Ok(c) = Conpty::load(&path) {
                return Some(c);
            }
        }

        // 2. Sibling of this binary (most convenient for users: just copy conpty.dll next to it)
        if let Ok(exe) = std::env::current_exe() {
            let sibling = exe.with_file_name("conpty.dll");
            if let Ok(c) = Conpty::load(sibling.to_str()?) {
                return Some(c);
            }
        }

        // 3. Windows Terminal (Microsoft Store install puts it in WindowsApps)
        let local_app = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let wt = format!(r"{}\Microsoft\WindowsApps\conpty.dll", local_app);
        if let Ok(c) = Conpty::load(&wt) {
            return Some(c);
        }

        // 4. WezTerm ships its own conpty.dll
        if let Ok(c) = Conpty::load(r"C:\Program Files\WezTerm\conpty.dll") {
            return Some(c);
        }

        None
    }

    // ── ConPTY loader (Warp's conpty_api.rs) ────────────────────────────────

    type CreateFn =
        unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> windows::core::HRESULT;
    type CloseFn = unsafe extern "system" fn(HPCON);

    struct Conpty {
        create: CreateFn,
        close: CloseFn,
    }

    impl Conpty {
        unsafe fn load(path: &str) -> windows::core::Result<Self> {
            let h = HSTRING::from(path);
            let m = LoadLibraryW(PCWSTR::from_raw(h.as_ptr()))?;
            macro_rules! sym {
                ($name:literal, $ty:ty) => {
                    mem::transmute::<_, $ty>(
                        GetProcAddress(m, windows::core::s!($name))
                            .expect(concat!("conpty.dll missing ", $name)),
                    )
                };
            }
            Ok(Conpty {
                create: sym!("CreatePseudoConsole", CreateFn),
                close: sym!("ClosePseudoConsole", CloseFn),
            })
        }

        /// Exact copy of Warp's conpty_api.rs::create:
        /// pass the SAME handle for both hInput and hOutput, then free our copy.
        unsafe fn create_pty(&self, size: COORD, pipe: HANDLE) -> windows::core::Result<HPCON> {
            let mut pty = HPCON::default();
            (self.create)(size, pipe, pipe, 0, &mut pty).ok()?;
            let _ = CloseHandle(pipe);
            Ok(pty)
        }
    }

    // ── ProcThreadAttributeList (Warp's proc_thread_attribute_list.rs) ──────

    struct AttrList {
        data: Box<[u8]>,
    }

    impl AttrList {
        unsafe fn new() -> windows::core::Result<Self> {
            let mut bytes: usize = 0;
            let _ = InitializeProcThreadAttributeList(None, 1, None, &mut bytes);
            let mut data: Box<[u8]> = vec![0u8; bytes].into_boxed_slice();
            let ptr = LPPROC_THREAD_ATTRIBUTE_LIST(data.as_mut_ptr() as _);
            InitializeProcThreadAttributeList(Some(ptr), 1, None, &mut bytes)?;
            Ok(Self { data })
        }
        fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            LPPROC_THREAD_ATTRIBUTE_LIST(self.data.as_mut_ptr() as _)
        }
        unsafe fn set_conpty(&mut self, pty: HPCON) -> windows::core::Result<()> {
            UpdateProcThreadAttribute(
                self.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(pty.0 as _),
                size_of::<HPCON>(),
                None,
                None,
            )
        }
    }
    impl Drop for AttrList {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.ptr()) };
        }
    }

    // ── Process spawn (Warp's windows/mod.rs) ───────────────────────────────

    fn spawn_bash(distro: Option<&str>, pty: HPCON) -> windows::core::Result<PROCESS_INFORMATION> {
        let cmd_s = match distro {
            Some(d) => format!("wsl.exe --distribution {} -- bash --norc --noprofile", d),
            None => "wsl.exe -- bash --norc --noprofile".to_owned(),
        };
        let mut cmd_wide: Vec<u16> = OsString::from(&cmd_s)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut attrs = unsafe { AttrList::new()? };
        unsafe { attrs.set_conpty(pty)? };
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attrs.ptr();
        let mut pi = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessW(
                PCWSTR::null(),
                Some(windows::core::PWSTR(cmd_wide.as_mut_ptr())),
                None,
                None,
                false,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                None,
                PCWSTR::null(),
                &si.StartupInfo as *const STARTUPINFOW,
                &mut pi,
            )?;
        }
        println!("  Spawned: {}  PID={}", cmd_s, pi.dwProcessId);
        Ok(pi)
    }

    // ── mio event loop (Warp's event_loop.rs) ───────────────────────────────

    const TOK: Token = Token(0);

    fn event_loop(server: HANDLE, data: &[u8], deadline: Instant) -> io::Result<usize> {
        let mut pipe = unsafe { NamedPipe::from_raw_handle(server.0 as *mut _) };
        let mut poll = Poll::new()?;
        poll.registry()
            .register(&mut pipe, TOK, Interest::READABLE | Interest::WRITABLE)?;

        let mut events = Events::with_capacity(64);
        let mut write_pos = 0usize;
        let mut total_written = 0usize;
        let mut can_read = true;
        let mut can_write = true;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            poll.poll(&mut events, Some(remaining.min(Duration::from_millis(50))))?;
            for ev in &events {
                if ev.token() == TOK {
                    if ev.is_readable() {
                        can_read = true;
                    }
                    if ev.is_writable() {
                        can_write = true;
                    }
                }
            }
            if can_read {
                let mut buf = [0u8; 4096];
                loop {
                    match pipe.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            can_read = false;
                            break;
                        }
                        Ok(_) => {}
                    }
                }
            }
            if can_write && write_pos < data.len() {
                match pipe.write(&data[write_pos..]) {
                    Ok(n) => {
                        write_pos += n;
                        total_written += n;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        can_write = false;
                    }
                    Err(e) => {
                        eprintln!("  write error at {}: {}", write_pos, e);
                        break;
                    }
                }
            }
            if write_pos >= data.len() {
                break;
            }
        }
        Ok(total_written)
    }

    // ── Main ─────────────────────────────────────────────────────────────────

    // Total content size matches Warp's shell integration script (~80 KB).
    // 200-char lines with a shorter last line (same as the first working test).
    // This exact byte layout is known to let EOM through on pre-release WSL
    // while still dropping content in the middle.
    const CONTENT: usize = 81_920; // total 'A' bytes in the heredoc
    const LINE_LEN: usize = 200; // chars per full line
    const RESULT: &str = "/tmp/wsl_repro.txt";

    pub fn run(distro: Option<&str>) {
        // Build lines: full 200-char lines then a shorter remainder if needed
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut left = CONTENT;
        while left > 0 {
            let n = left.min(LINE_LEN);
            lines.push(vec![b'A'; n]);
            left -= n;
        }
        let n_lines = lines.len();
        // Content bytes = sum of line lengths + one newline per line
        let content_bytes = CONTENT + n_lines;

        println!("============================================================");
        println!("WSL ConPTY stdin truncation repro (Rust)");
        println!(
            "  Content : {} A-chars in {} lines ({} bytes with newlines)",
            CONTENT, n_lines, content_bytes
        );
        println!("  Distro  : {}", distro.unwrap_or("(default)"));
        println!("============================================================\n");

        // 1. Bidirectional pipe
        let (client, server) = unsafe { create_duplex_pipe().expect("create_duplex_pipe") };

        // 2. ConPTY — same handle for both hInput and hOutput (Warp's approach)
        let conpty = unsafe {
            find_conpty_dll().expect(
                "conpty.dll not found. Copy it to the same directory as this binary, \
                 or set CONPTY_DLL_PATH.",
            )
        };
        let pty = unsafe {
            conpty
                .create_pty(COORD { X: 220, Y: 50 }, client)
                .expect("CreatePseudoConsole")
        };

        // 3. Spawn wsl.exe -- bash as ConPTY child
        let pi = spawn_bash(distro, pty).expect("spawn_bash");
        unsafe {
            let _ = CloseHandle(pi.hThread);
        }
        println!("Waiting 3 s for bash to start…");
        std::thread::sleep(Duration::from_secs(3));

        // 4. Build heredoc using the pre-computed lines.
        let mut hd: Vec<u8> = Vec::new();
        hd.extend_from_slice(b" read -r -d '' VAR << 'EOM'\n");
        for (i, line) in lines.iter().enumerate() {
            hd.extend_from_slice(line);
            if i + 1 < n_lines {
                hd.push(b'\n');
            }
        }
        hd.extend_from_slice(b"\nEOM\n");
        // echo ${#VAR} writes a single small number — no stdout backpressure
        hd.extend_from_slice(format!(" echo ${{#VAR}} > {}\n", RESULT).as_bytes());
        hd.extend_from_slice(b" exit\n");

        println!("Sending {} raw bytes via ConPTY stdin…", hd.len());

        // 5. mio event loop — reads ConPTY output and writes stdin concurrently
        let deadline = Instant::now() + Duration::from_secs(30);
        let written = event_loop(server, &hd, deadline).unwrap_or(0);
        println!(
            "Event loop done: {}/{} raw bytes delivered.\n",
            written,
            hd.len()
        );

        // 6. Wait for bash to exit
        println!("Waiting up to 30 s for bash to exit…");
        let w = unsafe { WaitForSingleObject(pi.hProcess, 30_000) };
        let timed_out = w.0 == 0x00000102;
        unsafe {
            let _ = CloseHandle(pi.hProcess);
            (conpty.close)(pty);
        }

        if timed_out {
            println!();
            println!("  [BUG] bash timed out — EOM was dropped.");
            println!("  wsl.exe is not delivering all ConPTY stdin bytes.");
            return;
        }

        // 7. Read result file via plain wsl (not ConPTY — known to work)
        let mut args: Vec<String> = Vec::new();
        if let Some(d) = distro {
            args.extend_from_slice(&["--distribution".into(), d.into()]);
        }
        args.extend_from_slice(&["--".into(), "cat".into(), RESULT.into()]);
        let out = Command::new(r"C:\Windows\System32\wsl.exe")
            .args(&args)
            .output()
            .expect("wsl cat");
        let received = String::from_utf8_lossy(&out.stdout);

        // bash wrote ${#VAR} — the character count of what it received.
        // Expected if all bytes arrived: content_bytes (A's + newlines).
        let bash_count: usize = received.trim().parse().unwrap_or(0);
        let dropped_bytes = content_bytes.saturating_sub(bash_count);
        // Estimate lines dropped (each full line = LINE_LEN + 1 chars)
        let lines_received = bash_count / (LINE_LEN + 1);
        let received_bytes = bash_count;

        println!();
        println!("============================================================");
        println!("RESULTS");
        println!("  Lines sent     : {}", n_lines);
        println!("  Lines received : ~{}", lines_received);
        println!("  Bytes sent     : {}", content_bytes);
        println!("  Bytes received : {}", received_bytes);

        if bash_count >= content_bytes {
            println!("============================================================");
            println!(
                "  [OK] All {} chars received. Bug not reproduced.",
                bash_count
            );
            return;
        }

        println!(
            "  Lines dropped  : ~{} ({:.1}%)",
            n_lines.saturating_sub(lines_received),
            dropped_bytes as f64 / content_bytes as f64 * 100.0
        );
        println!(
            "  Bytes dropped  : {} ({:.1}%)",
            dropped_bytes,
            dropped_bytes as f64 / content_bytes as f64 * 100.0
        );
        println!("============================================================");
        println!("  [BUG CONFIRMED]");
    }
}

fn main() {
    let distro = std::env::args().nth(1);

    #[cfg(windows)]
    repro::run(distro.as_deref());

    #[cfg(not(windows))]
    eprintln!("Windows only.");
}
