# WSL ConPTY stdin truncation repro

Minimal standalone repro for a regression in WSL 2.9.x pre-release where
bytes written to a ConPTY's stdin pipe are silently dropped mid-stream.

## Quick repro

**Requirements:**
- Windows 10 1903+ or Windows 11 with WSL 2 and a Linux distro installed
- Rust toolchain (`cargo`)
- No other dependencies — uses `kernel32!CreatePseudoConsole` which ships
  with Windows

```powershell
git clone https://github.com/warpdotdev/wsl-conpty-stdin-repro
cd wsl-conpty-stdin-repro
cargo build
.\target\debug\wsl-stdin-repro.exe Ubuntu
```

### Output on WSL 2.9.3 (broken)

```
============================================================
WSL ConPTY stdin truncation repro
  Content : 81920 A-chars in 410 lines (82330 bytes with newlines)
  Distro  : Ubuntu
============================================================

  Spawned: wsl.exe --distribution Ubuntu -- bash --norc --noprofile  PID=2512
Waiting 3 s for bash to start…
Sending 82403 raw bytes via ConPTY stdin…
Write done: 82403/82403 raw bytes delivered.

Waiting up to 30 s for bash to exit…

============================================================
RESULTS
  Lines sent     : 410
  Lines received : ~300
  Bytes sent     : 82330
  Bytes received : 60313
  Lines dropped  : ~110 (26.7%)
  Bytes dropped  : 22017 (26.7%)
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
| 2.9.3       | 82,330    | 60,313 ✗       | 22,017 bytes dropped (26.7%) |

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

1. Two anonymous pipes are created: one for stdin, one for stdout
2. `kernel32!CreatePseudoConsole` is called with the read end of the
   stdin pipe and the write end of the stdout pipe
3. `wsl.exe -- bash` is spawned as a ConPTY child
4. Stdin is written and stdout is drained concurrently in separate threads
5. bash runs `read -r -d '' VAR << 'EOM' ... EOM; echo ${#VAR} > /tmp/wsl_repro.txt`
6. Result is read back via a plain (non-ConPTY) `wsl -- cat` invocation

Note: Warp itself uses a slightly different setup
(`conpty.dll` + a single bidirectional named pipe), but the bug reproduces
with the standard two-pipe kernel32 API as well, which rules out Warp's
pipe configuration as a factor.

## Related

- WSL issue: [microsoft/WSL#XXXXX](https://github.com/microsoft/WSL/issues)
- Warp: https://www.warp.dev
