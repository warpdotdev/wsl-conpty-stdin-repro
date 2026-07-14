//! WSL ConPTY stdin truncation repro
//!
//! Sends ~82 KB to bash's stdin via a ConPTY and checks how many bytes arrive.
//!
//! Usage: wsl-stdin-repro [DISTRO_NAME]

#[cfg(windows)]
#[allow(unsafe_op_in_unsafe_fn)]
mod repro {
    use std::ffi::OsString;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::process::Command;
    use std::time::Duration;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::SECURITY_ATTRIBUTES;
    use windows::Win32::System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON};
    use windows::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
        STARTUPINFOEXW, STARTUPINFOW, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject,
    };
    use windows::core::PCWSTR;

    /// HANDLE is a raw pointer and not Send by default; this wrapper
    /// asserts it is safe to transfer across threads for our use case
    /// (each handle is owned by exactly one thread at a time).
    struct SendHandle(HANDLE);
    unsafe impl Send for SendHandle {}

    unsafe fn make_pipe() -> windows::core::Result<(HANDLE, HANDLE)> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: true.into(),
            ..Default::default()
        };
        let mut r = HANDLE::default();
        let mut w = HANDLE::default();
        windows::Win32::System::Pipes::CreatePipe(&mut r, &mut w, Some(&sa), 0)?;
        Ok((r, w))
    }

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

    unsafe fn spawn_bash(
        distro: Option<&str>,
        pty: HPCON,
    ) -> windows::core::Result<PROCESS_INFORMATION> {
        let cmd_s = match distro {
            Some(d) => format!("wsl.exe --distribution {} -- bash --norc --noprofile", d),
            None => "wsl.exe -- bash --norc --noprofile".to_owned(),
        };
        let mut cmd_wide: Vec<u16> = OsString::from(&cmd_s)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut attrs = AttrList::new()?;
        attrs.set_conpty(pty)?;
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attrs.ptr();
        let mut pi = PROCESS_INFORMATION::default();
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
        println!("  Spawned: {}  PID={}", cmd_s, pi.dwProcessId);
        Ok(pi)
    }

    /// Write all bytes to a handle, returning the total written.
    /// Takes ownership of a SendHandle so it can be passed to a thread.
    fn write_all_blocking(sh: SendHandle, data: Vec<u8>) -> usize {
        let handle = sh.0;
        let mut written = 0usize;
        while written < data.len() {
            let mut n = 0u32;
            let ok = unsafe {
                windows::Win32::Storage::FileSystem::WriteFile(
                    handle,
                    Some(&data[written..]),
                    Some(&mut n),
                    None,
                )
                .is_ok()
            };
            if !ok || n == 0 {
                break;
            }
            written += n as usize;
        }
        written
    }

    /// Drain a handle to /dev/null, preventing ConPTY stdout backpressure.
    /// Takes ownership of a SendHandle so it can be passed to a thread.
    fn drain_blocking(sh: SendHandle) {
        let handle = sh.0;
        let mut buf = vec![0u8; 4096];
        loop {
            let mut n = 0u32;
            let ok = unsafe {
                windows::Win32::Storage::FileSystem::ReadFile(
                    handle,
                    Some(&mut buf),
                    Some(&mut n),
                    None,
                )
                .is_ok()
            };
            if !ok || n == 0 {
                break;
            }
        }
    }

    const CONTENT: usize = 81_920;
    const LINE_LEN: usize = 200;
    const RESULT: &str = "/tmp/wsl_repro.txt";

    pub fn run(distro: Option<&str>) {
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut left = CONTENT;
        while left > 0 {
            let n = left.min(LINE_LEN);
            lines.push(vec![b'A'; n]);
            left -= n;
        }
        let n_lines = lines.len();
        let content_bytes = CONTENT + n_lines; // A-chars + one newline per line

        println!("============================================================");
        println!("WSL ConPTY stdin truncation repro");
        println!(
            "  Content : {} A-chars in {} lines ({} bytes with newlines)",
            CONTENT, n_lines, content_bytes
        );
        println!("  Distro  : {}", distro.unwrap_or("(default)"));
        println!("============================================================\n");

        unsafe {
            // 1. Create the two anonymous pipes
            //    stdin_read  / stdin_write  : we write to stdin_write
            //    stdout_read / stdout_write : we read from stdout_read
            let (stdin_read, stdin_write) = make_pipe().expect("stdin pipe");
            let (stdout_read, stdout_write) = make_pipe().expect("stdout pipe");

            // 2. CreatePseudoConsole — standard kernel32 API, uses system conhost.exe
            let pty: HPCON = CreatePseudoConsole(
                COORD { X: 220, Y: 50 },
                stdin_read,   // conhost reads stdin from here
                stdout_write, // conhost writes stdout to here
                0,
            )
            .expect("CreatePseudoConsole");
            // ConPTY inherited both ends; close our copies
            let _ = CloseHandle(stdin_read);
            let _ = CloseHandle(stdout_write);

            // 3. Spawn wsl.exe -- bash as ConPTY child
            let pi = spawn_bash(distro, pty).expect("spawn_bash");
            let _ = CloseHandle(pi.hThread);

            println!("Waiting 5s for bash to start…");
            std::thread::sleep(Duration::from_secs(5));

            // 4. Build heredoc
            let mut hd: Vec<u8> = Vec::new();
            hd.extend_from_slice(b" read -r -d '' VAR << 'EOM'\n");
            for (i, line) in lines.iter().enumerate() {
                hd.extend_from_slice(line);
                if i + 1 < n_lines {
                    hd.push(b'\n');
                }
            }
            hd.extend_from_slice(b"\nEOM\n");
            hd.extend_from_slice(format!(" echo ${{#VAR}} > {}\n", RESULT).as_bytes());
            hd.extend_from_slice(b" exit\n");

            println!("Sending {} raw bytes via ConPTY stdin…", hd.len());

            // 5. Drain stdout in one thread, write stdin in another.
            //    Both must run concurrently to avoid deadlocking the ConPTY.
            // Wrap handles before spawning — SendHandle: Send, raw HANDLE: not Send
            let stdout_sh = SendHandle(stdout_read);
            let stdout_thread = std::thread::spawn(move || drain_blocking(stdout_sh));

            let hd_len = hd.len();
            let stdin_sh = SendHandle(stdin_write);
            let stdin_thread = std::thread::spawn(move || write_all_blocking(stdin_sh, hd));
            let written = stdin_thread.join().unwrap_or(0);
            println!("Write done: {}/{} raw bytes delivered.\n", written, hd_len);

            // 6. Wait for bash to exit
            println!("Waiting up to 5 s for bash to exit…");
            let w = WaitForSingleObject(pi.hProcess, 5_000);
            let timed_out = w.0 == 0x00000102;
            if timed_out {
                let _ = TerminateProcess(pi.hProcess, 1);
            }
            let _ = CloseHandle(pi.hProcess);
            ClosePseudoConsole(pty);
            let _ = CloseHandle(stdout_read);
            stdout_thread.join().ok();
            let _ = CloseHandle(stdin_write);

            if timed_out {
                println!("  [BUG] bash timed out — EOM was dropped by wsl.exe.");
                return;
            }

            // 7. Read result via plain wsl (not ConPTY — known to work)
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

            let bash_count: usize = received.trim().parse().unwrap_or(0);
            // bash's ${#VAR} excludes the heredoc's final trailing newline,
            // so the expected count on a working system is content_bytes - 1.
            let expected = content_bytes.saturating_sub(1);
            let dropped = expected.saturating_sub(bash_count);

            println!();
            println!("============================================================");
            println!("RESULTS");
            println!("  Bytes sent     : {}", expected);
            println!("  Bytes received : {}", bash_count);

            if bash_count >= expected {
                println!("============================================================");
                println!("  [OK] All bytes received. Bug not reproduced.");
                return;
            }

            println!(
                "  Bytes dropped  : {} ({:.1}%)",
                dropped,
                dropped as f64 / expected as f64 * 100.0
            );
            println!("============================================================");
            println!("  [BUG CONFIRMED]");
        }
    }
}

fn main() {
    let distro = std::env::args().nth(1);

    #[cfg(windows)]
    repro::run(distro.as_deref());

    #[cfg(not(windows))]
    eprintln!("Windows only.");
}
