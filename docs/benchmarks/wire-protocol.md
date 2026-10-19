# Own Wayland wire protocol vs wayland-client

**Question:** is it worth speaking the Wayland wire protocol ourselves
instead of depending on wayland-client (and SCTK)?

**Verdict:** yes, but for dependencies, binary size and transparency, not
for speed. Latency is the same.

## Method

Spike: [`spikes/wire/`](../../spikes/wire). The only dependency is `libc`,
needed for `SCM_RIGHTS` fd passing (`sendmsg`/`recvmsg`; the std API for it
is unstable). There is no code generation: opcodes and signatures were
extracted from the protocol XMLs. It implements the full `vp` path (layer
surface, viewporter, subsurface, seat, presentation) **plus** data device
and primary selection (MIME lists) and a lazy 4 KB peek. Compared against
`latency/surf vp` (wayland-client, without the selection handling). 20 runs.

## Results (p50)

| | own wire (libc) | wayland-client |
|---|---|---|
| dependencies | **1** | 18 |
| binary | **344 KB** | 607 KB |
| configure | 0.33 ms | 0.33 ms |
| cursor + focus | 1.29 ms | 1.14 ms |
| wheel commit | 1.58 ms | 1.41 ms |
| presented p10 / p50 | 10.3 / 18.6 ms | 10.2 / 18.6 ms |
| peak RSS (HWM) | **3.3 MB** | 3.55 MB |

Size of the code: ~300 lines for the protocol layer (socket, framing, fd
passing) and ~250 lines for the whole flow.

**Cost of one complete invocation** (own wire, 15 runs): **~860 µs CPU**
from exec to exit (min 767, max 923), and ~366 minor page faults. About 225
of those faults come from the 0.9 MB wheel buffer, which can be cut with
`MAP_POPULATE` or by touching only the ring's rows.

*Later:* this spike fills the whole buffer and launches from a shell. With
the real wheel (only the ring's pixels are written) and spawned by niri,
see [latency.md](latency.md#startup-cpu-ours-vs-the-spawners): ~0.47 ms
CPU from `main` to the wheel commit, plus a spawner-dependent cost before
`main`. `MAP_POPULATE` turned out no cheaper than the faults it saves.

| opt-level | binary |
|---|---|
| 3 | 344 KB |
| s | 324 KB |
| z | 325 KB |

The rest of the binary is mostly std. Going no-std or libc-free is not
worth it: libc is already mapped in every process, as shared pages.

**Risk:** without codegen, a wrong opcode is a silent bug. Opcodes must be
verified against the protocol XMLs at build or test time.

## Reproduce

```sh
cd spikes/wire && cargo build --release && ./target/release/wire
```
