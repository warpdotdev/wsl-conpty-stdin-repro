//! WSL ConPTY stdin truncation repro (Rust)
//!
//! Ports the relevant parts of Warp's Windows PTY stack:
//!   - pipes.rs                      : bidirectional named pipe via NtCreateNamedPipeFile
//!   - conpty_api.rs                 : loads conpty.dll, passes SAME handle for hInput+hOutput
//!   - proc_thread_attribute_list.rs : ProcThreadAttributeList wrapper
//!   - windows/mod.rs                : CreateProcessW with ConPTY attribute
//!   - event_loop.rs                 : mio read+write loop with WouldBlock handling
//!
//! Usage: wsl-stdin-repro [SIZE_BYTES] [DISTRO_NAME]
//!   Default: 81920 bytes, default distro

#[cfg(windows)]
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

    use windows::core::{HSTRING, PCWSTR};
    use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows::Wdk::Storage::FileSystem::{
        NtCreateFile, FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_PIPE_BYTE_STREAM_MODE, FILE_PIPE_BYTE_STREAM_TYPE, FILE_PIPE_QUEUE_OPERATION,
        FILE_SYNCHRONOUS_IO_NONALERT, NTCREATEFILE_CREATE_OPTIONS,
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
        CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
        UpdateProcThreadAttribute, WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT,
        EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTUPINFOEXW, STARTUPINFOW,
    };
    use windows::Win32::System::WindowsProgramming::RtlInitUnicodeString;

    // ── Bidirectional pipe (Warp's pipes.rs) ────────────────────────────────

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

    /// Bidirectional pipe exactly as Warp's pipes.rs.
    /// Returns (client, server):
    ///   client → passed to ConPTY as BOTH hInput and hOutput
    ///   server → host reads output AND writes input via mio
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
            FILE_CREATE.0,                         // CreateDisposition
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

    // ── conpty.dll finder ────────────────────────────────────────────────────

    /// Search well-known locations for conpty.dll.
    /// Prefers CONPTY_DLL_PATH env var if set.
    unsafe fn find_conpty_dll() -> Option<Conpty> {
        // 1. Env var override
        if let Ok(path) = std::env::var("CONPTY_DLL_PATH") {
            if let Ok(c) = Conpty::load(&path) { return Some(c); }
        }

        // 2. Same directory as our binary
        if let Ok(exe) = std::env::current_exe() {
            let sibling = exe.with_file_name("conpty.dll");
            if let Ok(c) = Conpty::load(sibling.to_str()?) { return Some(c); }
        }

        // 3. Windows Terminal (Microsoft Store)
        let local_app = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let wt_glob = format!(r"{}\Microsoft\WindowsApps\conpty.dll", local_app);
        if let Ok(c) = Conpty::load(&wt_glob) { return Some(c); }

        // 4. WezTerm
        let wezterm = r"C:\Program Files\WezTerm\conpty.dll";
        if let Ok(c) = Conpty::load(wezterm) { return Some(c); }

        // 5. Warp dev build paths (developer convenience)
        for path in &[
            r"C:\Users\dev\warp\warp\target\debug\conpty.dll",
            r"C:\Users\dev\warp\warp\app\assets\windows\x64\conpty.dll",
        ] {
            if let Ok(c) = Conpty::load(path) { return Some(c); }
        }

        None
    }

    // ── ConPTY loader (Warp's conpty_api.rs) ────────────────────────────────

    type CreateFn  = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON)
        -> windows::core::HRESULT;
    type CloseFn   = unsafe extern "system" fn(HPCON);
    type ReleaseFn = unsafe extern "system" fn(HPCON) -> windows::core::HRESULT;

    struct Conpty { create: CreateFn, close: CloseFn, release: ReleaseFn }

    impl Conpty {
        unsafe fn load(path: &str) -> windows::core::Result<Self> {
            let h = HSTRING::from(path);
            let m = LoadLibraryW(PCWSTR::from_raw(h.as_ptr()))?;
            macro_rules! sym {
                ($name:literal, $ty:ty) => {
                    mem::transmute::<_, $ty>(
                        GetProcAddress(m, windows::core::s!($name))
                            .expect(concat!("conpty.dll missing ", $name))
                    )
                };
            }
            Ok(Conpty {
                create:  sym!("CreatePseudoConsole",  CreateFn),
                close:   sym!("ClosePseudoConsole",   CloseFn),
                release: sym!("ReleasePseudoConsole", ReleaseFn),
            })
        }

        /// Exact copy of Warp's conpty_api.rs::create:
        /// pass the SAME handle for both hInput and hOutput, then free our copy.
        unsafe fn create_pty(&self, size: COORD, mut pipe: HANDLE) -> windows::core::Result<HPCON> {
            let mut pty = HPCON::default();
            (self.create)(size, pipe, pipe, 0, &mut pty).ok()?;
            let _ = CloseHandle(pipe); // ConPTY takes sole ownership
            Ok(pty)
        }
    }

    // ── ProcThreadAttributeList (Warp's proc_thread_attribute_list.rs) ──────

    struct AttrList { data: Box<[u8]> }

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
                self.ptr(), 0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(pty.0 as _),
                size_of::<HPCON>(),
                None, None,
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
            None    => "wsl.exe -- bash --norc --noprofile".to_owned(),
        };
        let mut cmd_wide: Vec<u16> = OsString::from(&cmd_s).encode_wide().chain(Some(0)).collect();

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
                None, None, false,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                None, PCWSTR::null(),
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
        // SAFETY: we own server and keep it alive for the duration of this function
        let mut pipe = unsafe { NamedPipe::from_raw_handle(server.0 as *mut _) };
        let mut poll = Poll::new()?;
        poll.registry().register(&mut pipe, TOK, Interest::READABLE | Interest::WRITABLE)?;

        let mut events = Events::with_capacity(64);
        let mut write_pos = 0usize;
        let mut total_written = 0usize;
        let mut can_read  = true;
        let mut can_write = true;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { break; }

            poll.poll(&mut events, Some(remaining.min(Duration::from_millis(50))))?;
            for ev in &events {
                if ev.token() == TOK {
                    if ev.is_readable() { can_read  = true; }
                    if ev.is_writable() { can_write = true; }
                }
            }

            // Drain output (keeps bash from blocking on its stdout)
            if can_read {
                let mut buf = [0u8; 4096];
                loop {
                    match pipe.read(&mut buf) {
                        Ok(0) | Err(_) => { can_read = false; break; }
                        Ok(_) => {}
                    }
                }
            }

            // Write next chunk to stdin
            if can_write && write_pos < data.len() {
                match pipe.write(&data[write_pos..]) {
                    Ok(n)  => { write_pos += n; total_written += n; }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => { can_write = false; }
                    Err(e) => { eprintln!("  write error at {}: {}", write_pos, e); break; }
                }
            }

            if write_pos >= data.len() { break; }
        }
        Ok(total_written)
    }

    // ── Gap finder ───────────────────────────────────────────────────────────
    //
    // Sends 500 lines numbered 000000..499999 (each exactly 8 bytes: "NNNNNN\n").
    // Writes $VAR to a file and checks which line numbers are present / absent.
    // Run: wsl-stdin-repro 0 Ubuntu --find-gap

    pub fn find_gap(distro: Option<&str>) {
        println!("Finding gap: 500 numbered lines → 4000 bytes total\n");

        let (client, server) = unsafe { create_duplex_pipe().expect("pipe") };
        let conpty = unsafe { find_conpty_dll().expect("conpty.dll") };
        let pty = unsafe { conpty.create_pty(COORD { X: 220, Y: 50 }, client).expect("ConPTY") };
        let pi  = spawn_bash(distro, pty).expect("bash");
        unsafe { let _ = CloseHandle(pi.hThread); }

        std::thread::sleep(Duration::from_secs(3));

        // Build content: 500 lines, each "NNNNNN\n" (line number zero-padded)
        // That's 500 * 7 = 3500 bytes of content.
        let n_lines: usize = 2500;  // 2500 * 7 = 17.5 KB
        let result_file = "/tmp/wsl_gap.txt";

        let mut content = Vec::<u8>::new();
        for i in 0..n_lines {
            content.extend_from_slice(format!("{:06}\n", i).as_bytes());
        }
        let content_len = content.len(); // should be 3500

        let mut hd: Vec<u8> = Vec::new();
        hd.extend_from_slice(b" read -r -d '' VAR << 'EOM'\n");
        hd.extend_from_slice(&content);
        hd.extend_from_slice(b"EOM\n");
        // Write VAR to file, then exit
        hd.extend_from_slice(format!(" printf '%s' \"$VAR\" > {}\n", result_file).as_bytes());
        hd.extend_from_slice(b" exit\n");

        let deadline = Instant::now() + Duration::from_secs(20);
        let _written = event_loop(server, &hd, deadline).unwrap_or(0);

        let w = unsafe { WaitForSingleObject(pi.hProcess, 20_000) };
        unsafe { let _ = CloseHandle(pi.hProcess); (conpty.close)(pty); }

        if w.0 == 0x00000102 {
            println!("  [BUG] bash timed out — EOM never arrived.");
            return;
        }

        // Read the result file
        let mut args: Vec<String> = Vec::new();
        if let Some(d) = distro { args.extend_from_slice(&["--distribution".into(), d.into()]); }
        args.extend_from_slice(&["--".into(), "cat".into(), result_file.into()]);
        let out = Command::new(r"C:\Windows\System32\wsl.exe").args(&args).output().expect("cat");
        let received = String::from_utf8_lossy(&out.stdout);

        // Parse which line numbers are present
        let present: std::collections::BTreeSet<usize> = received
            .split('\n')
            .filter_map(|s| s.trim().parse().ok())
            .collect();

        println!("Content sent   : {} bytes ({} lines)", content_len, n_lines);
        println!("Bytes received : {}", received.len());
        println!("Lines received : {}", present.len());

        if present.len() == n_lines {
            println!("  [OK] All {} lines present — no drops at this size.", n_lines);
            return;
        }

        // Find first missing line
        let first_missing = (0..n_lines).find(|i| !present.contains(i));
        let last_present  = present.iter().next_back().copied();
        println!("  First missing line : {:?}  (byte offset ~{:?})",
            first_missing,
            first_missing.map(|i| i * 7));
        println!("  Last present line  : {:?}  (byte offset ~{:?})",
            last_present,
            last_present.map(|i| i * 7));

        // Print a short summary of the gap
        let missing: Vec<usize> = (0..n_lines).filter(|i| !present.contains(i)).collect();
        if missing.len() <= 10 {
            println!("  Missing lines: {:?}", missing);
        } else {
            println!("  Missing lines: {:?} … {:?} ({} total)",
                &missing[..5], &missing[missing.len()-5..], missing.len());
        }
    }

    // ── Main ─────────────────────────────────────────────────────────────────

    pub fn run(content_bytes: usize, distro: Option<&str>) {
        println!("============================================================");
        println!("WSL ConPTY stdin truncation repro (Rust)");
        println!("  Content bytes : {} ({:.1} KB)", content_bytes, content_bytes as f64 / 1024.0);
        println!("  Distro        : {}", distro.unwrap_or("(default)"));
        println!("============================================================\n");

        // 1. Bidirectional pipe (Warp's pipes.rs approach)
        let (client, server) = unsafe {
            create_duplex_pipe().expect("create_duplex_pipe")
        };
        println!("Pipe: client={:?}  server={:?}", client, server);

        // 2. Load conpty.dll; pass same handle as hInput AND hOutput (Warp's approach)
        // Search common locations: local dir, Windows Terminal, WezTerm, Warp dev builds
        let conpty = unsafe {
            find_conpty_dll().expect(
                "conpty.dll not found. Copy it from Windows Terminal or WezTerm \
                 to the same directory as this binary, or set CONPTY_DLL_PATH."
            )
        };
        let pty = unsafe {
            conpty.create_pty(COORD { X: 220, Y: 50 }, client).expect("CreatePseudoConsole")
        };
        println!("ConPTY: {:?}", pty);

        // 3. Spawn wsl.exe -- bash as ConPTY child
        let pi = spawn_bash(distro, pty).expect("spawn_bash");
        unsafe { let _ = CloseHandle(pi.hThread); }

        println!("Waiting 3 s for bash to start…");
        std::thread::sleep(Duration::from_secs(3));

        // 4. Build heredoc (same structure as Warp's bash.sh bootstrap)
        let result_file = "/tmp/wsl_repro.txt";
        let line_w = 200;
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut left = content_bytes;
        while left > 0 {
            let n = left.min(line_w);
            lines.push(vec![b'A'; n]);
            left -= n;
        }

        let mut hd: Vec<u8> = Vec::new();
        hd.extend_from_slice(b" read -r -d '' VAR << 'EOM'\n");
        for (i, line) in lines.iter().enumerate() {
            hd.extend_from_slice(line);
            if i + 1 < lines.len() { hd.push(b'\n'); }
        }
        hd.extend_from_slice(b"\nEOM\n");
        hd.extend_from_slice(format!(" echo ${{#VAR}} > {}\n", result_file).as_bytes());
        hd.extend_from_slice(b" exit\n");

        println!("Sending {} raw bytes (content={}) via ConPTY stdin…", hd.len(), content_bytes);

        // 5. mio event loop — reads bash output + writes heredoc concurrently
        let deadline = Instant::now() + Duration::from_secs(30);
        let written = event_loop(server, &hd, deadline).unwrap_or(0);
        println!("Event loop done: {}/{} raw bytes delivered.", written, hd.len());

        // 6. Wait for bash to exit (up to 30 s)
        println!("Waiting up to 30 s for bash to exit…");
        let w = unsafe { WaitForSingleObject(pi.hProcess, 30_000) };
        let timed_out = w.0 == 0x00000102; // WAIT_TIMEOUT
        unsafe {
            let _ = CloseHandle(pi.hProcess);
            (conpty.close)(pty);
        }

        if timed_out {
            println!();
            println!("  [BUG] bash timed out — EOM was dropped, bash is stuck waiting.");
            println!("  wsl.exe's StandardInputRelay is NOT delivering all stdin bytes.");
            return;
        }

        // 7. Read result via a plain wsl pipe invocation (no ConPTY, known to work)
        println!("\nReading {} via plain wsl…", result_file);
        let mut args: Vec<String> = Vec::new();
        if let Some(d) = distro {
            args.extend_from_slice(&["--distribution".into(), d.into()]);
        }
        args.extend_from_slice(&["--".into(), "cat".into(), result_file.into()]);
        let out = Command::new(r"C:\Windows\System32\wsl.exe")
            .args(&args)
            .output()
            .expect("wsl cat");
        let raw = String::from_utf8_lossy(&out.stdout);
        let received: Option<usize> = raw
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .next_back();

        println!();
        println!("============================================================");
        println!("RESULTS");
        println!("  Content bytes sent   : {}", content_bytes);
        println!("  Bytes bash recorded  : {:?}", received);
        println!("============================================================");
        match received {
            None => println!("  [INCONCLUSIVE] Could not read result file."),
            Some(n) if n < content_bytes => {
                let d = content_bytes - n;
                println!("  [BUG CONFIRMED] {} bytes dropped ({:.1}%)!", d, d as f64 / content_bytes as f64 * 100.0);
            }
            Some(_) => println!("  [OK] All bytes received. Bug not reproduced at this size."),
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let size: usize  = args.next().and_then(|s| s.parse().ok()).unwrap_or(81920);
    let distro = args.next();
    let find_gap = args.next().map(|s| s == "--find-gap").unwrap_or(false);

    #[cfg(windows)]
    if find_gap {
        repro::find_gap(distro.as_deref());
    } else {
        repro::run(size, distro.as_deref());
    }

    #[cfg(not(windows))]
    eprintln!("Windows only.");
}
