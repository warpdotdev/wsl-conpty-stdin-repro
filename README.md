# WSL ConPTY stdin truncation repro

Minimal standalone repro for a regression in WSL 2.9.x pre-release where
bytes written to a ConPTY's stdin pipe are silently dropped mid-stream.

## Quick repro

**Requirements:**
- Windows 10/11 with WSL 2 and a Linux distro installed
- Rust toolchain (`cargo`)
- `conpty.dll` — copy it from a [Windows Terminal](https://github.com/microsoft/terminal)
  installation (e.g. `%LOCALAPPDATA%\Microsoft\WindowsApps\conpty.dll`) or
  set `CONPTY_DLL_PATH` to its location. The binary also searches the same
  directory as the executable automatically.

```powershell
git clone https://github.com/warpdotdev/wsl-conpty-stdin-repro
cd wsl-conpty-stdin-repro
cargo build
copy "$env:LOCALAPPDATA\Microsoft\WindowsApps\conpty.dll" target\debug\
.\target\debug\wsl-stdin-repro.exe 81920 Ubuntu
```

### Output on WSL 2.9.3 (broken)

```
============================================================
WSL ConPTY stdin truncation repro (Rust)
  Content bytes : 81920 (80.0 KB)
  Distro        : Ubuntu
============================================================

  Spawned: wsl.exe --distribution Ubuntu -- bash --norc --noprofile  PID=2512
Waiting 3 s for bash to start…
Sending 82403 raw bytes (content=81920) via ConPTY stdin…
Event loop done: 82403/82403 raw bytes delivered.
Waiting up to 30 s for bash to exit…

============================================================
RESULTS
  Content bytes sent   : 81920
  Bytes bash received  : Some(62361)
============================================================
  [BUG CONFIRMED] 19559 bytes dropped (23.9%)!
```

### Output on WSL 2.7.10 (working)

```
============================================================
RESULTS
  Content bytes sent   : 81920
  Bytes bash received  : Some(82329)
============================================================
  [OK] All bytes received.
```

> **Note:** bash counts the 81,920 'A' characters **plus** the ~409 newlines
> between the 200-char lines in the heredoc, so the expected value on a
> working system is 82,329.

## Results summary

| WSL version | Bytes sent | Bytes received | Result |
|-------------|-----------|----------------|--------|
| 2.7.10      | 81,920    | 82,329 ✓       | All bytes delivered |
| 2.9.3       | 81,920    | 62,361 ✗       | 19,559 bytes dropped (23.9%) |

The dropped bytes are **not at the tail** of the write — delivery resumes
after the drop window, which means EOM and trailing commands still arrive.
The drop occurs in a burst partway through the stream, e.g.:

```
Content sent   : 17,500 bytes (2500 numbered lines)
Lines received : 2426 / 2500
First missing  : line 1678  (byte offset ~11,746)
Last present   : line 2499
Missing        : lines 1678–1751 (74 lines / 518 bytes)
```

## Background

[Warp](https://www.warp.dev) delivers its shell integration code to WSL
sessions by writing an ~80 KB script to the ConPTY's stdin pipe. On WSL
2.7.10 this works correctly. On WSL 2.9.x pre-release, bytes are silently
dropped causing shell integration to never complete.

## How the repro works

The repro uses the exact same ConPTY setup as Warp:

1. **Bidirectional named pipe** via `NtCreateNamedPipeFile`
   ([`pipes.rs`](https://github.com/warpdotdev/warp/blob/master/app/src/terminal/local_tty/windows/pipes.rs))
2. **`conpty.dll`** with the same pipe handle passed as both `hInput` and
   `hOutput`, giving the ConPTY sole ownership
   ([`conpty_api.rs`](https://github.com/warpdotdev/warp/blob/master/app/src/terminal/local_tty/windows/conpty_api.rs))
3. **`wsl.exe -- bash`** spawned as a ConPTY child
   ([`windows/mod.rs`](https://github.com/warpdotdev/warp/blob/master/app/src/terminal/local_tty/windows/mod.rs))
4. **mio async I/O loop** reading output and writing stdin concurrently
   with proper `WouldBlock` handling
   ([`event_loop.rs`](https://github.com/warpdotdev/warp/blob/master/app/src/terminal/local_tty/event_loop.rs))
5. bash runs `read -r -d '' VAR << 'EOM' ... EOM; echo ${#VAR} > /tmp/wsl_repro.txt`
6. Result is read back via a plain (non-ConPTY) `wsl -- cat` invocation

## Related

- WSL issue: [microsoft/WSL#XXXXX](https://github.com/microsoft/WSL/issues)
- Warp: https://www.warp.dev
