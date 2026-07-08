# WSL ConPTY stdin truncation repro

Minimal standalone repro for a regression in WSL 2.9.x pre-release where
bytes written to a ConPTY's stdin are silently dropped mid-stream.

## Background

[Warp](https://www.warp.dev) delivers its shell integration code to WSL sessions
by writing an ~80 KB script to the ConPTY's stdin pipe. On WSL 2.7.10 this
works correctly. On WSL 2.9.x pre-release, ~24% of the bytes are silently
dropped, causing WSL shell integration to never complete.

The drop is **not at the tail** of the write. It occurs in a burst window
partway through the stream (e.g. bytes 11,746–12,264 of a 17,500-byte write),
then delivery resumes — consistent with a momentary console input queue
overflow that clears after wsl.exe drains it.

## Suspected cause

Commit `5db2759f` in the WSL repo — *"Use overlapped IO when reading from
the console"* — refactored `StandardInputRelay` to use `MultiHandleWait` +
`ReadConsoleHandle` (a `RegisterWaitForSingleObject`-based loop). There
appears to be a race window between `m_handleSignaledEvent.ResetEvent()` and
the next `RegisterWaitForSingleObject` call. Console input events that arrive
during this window are never signaled, stalling the drain loop and causing
the Windows console input queue to overflow. When the queue overflows, events
are silently discarded.

The old implementation (a simple blocking `ReadConsoleInputExW` loop) kept
the queue continuously drained and did not have this problem.

## Results

| WSL version | Bytes sent | Bytes received | Result              |
|-------------|-----------|----------------|---------------------|
| 2.7.10      | 81,920    | 82,329 ✓       | All bytes delivered |
| 2.9.3       | 81,920    | 62,361 ✗       | 19,559 bytes dropped (23.9%) |

> **Note:** `${#VAR}` counts the 81,920 'A' bytes **plus** the ~409 newlines
> between the 200-char lines in the heredoc, so the expected value on a
> working system is 82,329, not 81,920.

Gap analysis (with 2500 numbered lines on 2.9.3):

```
Content sent   : 17,500 bytes (2500 lines)
Bytes received : 16,987
Lines received : 2426
First missing  : line 1678  (byte offset ~11,746)
Last present   : line 2499  (byte offset ~17,493)
Missing lines  : 1678–1751 (74 lines / 518 bytes)
```

## How the repro works

The repro ports the exact ConPTY setup that Warp uses:

1. **Bidirectional named pipe** — created with `NtCreateNamedPipeFile`
   (same as [`app/src/terminal/local_tty/windows/pipes.rs`](https://github.com/warpdotdev/warp/blob/master/app/src/terminal/local_tty/windows/pipes.rs))
2. **`conpty.dll`** — the same handle is passed as **both** `hInput` and
   `hOutput` to give the ConPTY sole ownership of the pipe
   (same as [`app/src/terminal/local_tty/windows/conpty_api.rs`](https://github.com/warpdotdev/warp/blob/master/app/src/terminal/local_tty/windows/conpty_api.rs))
3. **`wsl.exe -- bash`** spawned as a ConPTY child
   (same as [`app/src/terminal/local_tty/windows/mod.rs`](https://github.com/warpdotdev/warp/blob/master/app/src/terminal/local_tty/windows/mod.rs))
4. **mio async I/O loop** — reads output and writes stdin concurrently
   with proper `WouldBlock` handling
   (same as [`app/src/terminal/local_tty/event_loop.rs`](https://github.com/warpdotdev/warp/blob/master/app/src/terminal/local_tty/event_loop.rs))
5. bash runs `read -r -d '' VAR << 'EOM' ... EOM; echo ${#VAR} > /tmp/wsl_repro.txt`
6. The result is read back via a plain (non-ConPTY) `wsl -- cat` invocation

## Requirements

- Windows 10 / 11 with WSL 2 installed
- `conpty.dll` from [Windows Terminal](https://github.com/microsoft/terminal)
  or Warp — the repro looks for it at these paths in order:
  - `C:\Users\dev\warp\warp\target\debug\conpty.dll`
  - `C:\Users\dev\warp\warp\app\assets\windows\x64\conpty.dll`
  
  You can change the paths in `src/main.rs` or copy `conpty.dll`
  from a Windows Terminal installation to the same directory as the binary.
- Rust toolchain (`cargo`)

## Usage

```powershell
cargo build
# Test with default WSL distro
.\target\debug\wsl-stdin-repro.exe

# Test with a specific distro
.\target\debug\wsl-stdin-repro.exe 81920 Ubuntu

# Find which specific bytes are dropped (gap analysis)
.\target\debug\wsl-stdin-repro.exe 0 Ubuntu --find-gap
```

### Example output (WSL 2.9.3 — broken)

```
============================================================
WSL ConPTY stdin truncation repro (Rust)
  Content bytes : 81920 (80.0 KB)
  Distro        : Ubuntu
============================================================

Pipe: client=HANDLE(0x148)  server=HANDLE(0x144)
ConPTY: HPCON(...)
  Spawned: wsl.exe --distribution Ubuntu -- bash --norc --noprofile  PID=2512
Waiting 3 s for bash to start…
Sending 82403 raw bytes (content=81920) via ConPTY stdin…
Event loop done: 82403/82403 raw bytes delivered.
Waiting up to 30 s for bash to exit…

============================================================
RESULTS
  Content bytes sent   : 81920
  Bytes bash recorded  : Some(62361)
============================================================
  [BUG CONFIRMED] 19559 bytes dropped (23.9%)!
```

### Example output (WSL 2.7.10 — working)

```
============================================================
RESULTS
  Content bytes sent   : 81920
  Bytes bash recorded  : Some(82329)
============================================================
  [OK] All bytes received. Bug not reproduced at this size.
```

## Related

- WSL issue: [microsoft/WSL#XXXXX](https://github.com/microsoft/WSL/issues)
- Warp: https://www.warp.dev
