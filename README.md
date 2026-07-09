# WSL ConPTY stdin truncation repro

Minimal standalone repro for a regression in WSL 2.9.x pre-release where
bytes written to a ConPTY's stdin pipe are silently dropped mid-stream.

## Quick repro

**Requirements:**
- Windows 10/11 with WSL 2 and a Linux distro installed
- Rust toolchain (`cargo`)
- `conpty.dll` — **not** part of Windows itself; it is a redistributable
  from the [Windows Terminal / ConPTY project](https://github.com/microsoft/terminal).
  The binary searches for it automatically in these locations:
  1. `CONPTY_DLL_PATH` env var (explicit override)
  2. Same directory as the executable — easiest: just copy it next to the binary
  3. `%LOCALAPPDATA%\Microsoft\WindowsApps\conpty.dll` (some installs)
  4. `C:\Program Files\WezTerm\conpty.dll` — [WezTerm](https://wezfurlong.org/wezterm/) ships it

  If none of those match, download `conpty.dll` from the
  [Microsoft.Windows.Console.ConPTY NuGet package](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY)
  and copy it next to the binary.

```powershell
git clone https://github.com/warpdotdev/wsl-conpty-stdin-repro
cd wsl-conpty-stdin-repro
cargo build
# If WezTerm is installed the binary finds conpty.dll automatically.
# Otherwise copy conpty.dll next to the binary first:
# copy path\to\conpty.dll target\debug\
.\target\debug\wsl-stdin-repro.exe Ubuntu
```

### Output on WSL 2.9.3 (broken)

```
============================================================
WSL ConPTY stdin truncation repro (Rust)
  Content : 81920 A-chars in 410 lines (82330 bytes with newlines)
  Distro  : Ubuntu
============================================================

  Spawned: wsl.exe --distribution Ubuntu -- bash --norc --noprofile  PID=2512
Waiting 3 s for bash to start…
Sending 82403 raw bytes via ConPTY stdin…
Event loop done: 82403/82403 raw bytes delivered.

Waiting up to 30 s for bash to exit…

============================================================
RESULTS
  Lines sent     : 410
  Lines received : ~310
  Bytes sent     : 82330
  Bytes received : 62361
  Lines dropped  : ~100 (24.3%)
  Bytes dropped  : 19969 (24.3%)
============================================================
  [BUG CONFIRMED]
```

### Output on WSL 2.7.10 (working)

```
============================================================
RESULTS
  Lines sent     : 410
  Lines received : ~410
  Bytes sent     : 82330
  Bytes received : 82330
============================================================
  [OK] All 82330 chars received. Bug not reproduced.
```

> **Note:** bash counts the 81,920 'A' characters **plus** the 410 newlines
> (one per line) in the heredoc, so the expected total is 82,330.

## Results summary

| WSL version | Bytes sent | Bytes received | Result |
|-------------|-----------|----------------|--------|
| 2.7.10      | 82,330    | 82,330 ✓       | All bytes delivered |
| 2.9.3       | 82,330    | 62,361 ✗       | 19,969 bytes dropped (24.3%) |

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
