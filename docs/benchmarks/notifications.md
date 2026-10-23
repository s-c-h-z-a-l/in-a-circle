# Notifications: what the D-Bus client costs

**Question:** what does a run cost with `--notify`: our own CPU for the
notification path (connect, authenticate, `Hello`, `GetCapabilities`,
`Notify`), peak memory, and binary growth from the `libc`-only D-Bus client?

**Verdict:** _pending: measured on the developer machine, not yet filled in._
Target: the notification path ≤ 300 µs of our CPU, peak RSS ≤ 5 MB, growth
well under the plan's +100 KB allowance.

## Method

`INACIRCLE_MEASURE=1` prints, on stderr, `measure self_cpu_us=… vm_hwm=…`
before the notifications, and with `--notify` a further line
`measure notify_cpu_us=<n>`: our `getrusage(RUSAGE_SELF)` CPU from the start
of the final notice (open + `Notify`) to just before the Undo window. It
excludes the server's own work and costs nothing when the variable is unset.
`INACIRCLE_UNDO_WAIT_MS=0` skips the Undo window so a run ends at once. The
path sits outside the "`main` to exit ≤ 1 ms" budget (it runs after the
action has started) and is reported separately.

The bus is the real session bus with the real notification server
(noctalia), 20 runs of one small `.txt` converted to Markdown, p50 and max.
Peak RSS is `VmHWM` of the same runs. Binary size is the stripped release
`inacircle` against 650,472 B at the end of plan 2.

## Machine

The [reference machine](README.md#reference-machine), with the notification
server named above.

## Results

| | p50 | max |
|---|---|---|
| `notify_cpu_us` (final notice) | — | — |
| `self_cpu_us` (whole run, `--notify`) | — | — |
| `vm_hwm` (`--notify`) | — | — |
| `vm_hwm` (without `--notify`, for reference) | — | — |

| | bytes |
|---|---|
| `inacircle` at the end of plan 2 | 650,472 |
| `inacircle` now (stripped release) | — |
| growth | — |

## Reproduce

```sh
cargo build --release
R=$PWD/target/release
M=$(mktemp -d); mkdir -p $M/{actions,cache,state,home,config,files}
mkdir -m 700 $M/runtime
ln -s $R/inacircle-text $M/actions/
export INACIRCLE_ACTIONS_DIR=$M/actions XDG_CACHE_HOME=$M/cache \
  XDG_STATE_HOME=$M/state XDG_CONFIG_HOME=$M/config HOME=$M/home
# keep the real XDG_RUNTIME_DIR and DBUS_SESSION_BUS_ADDRESS: the real bus
cd $M/files

for i in $(seq 20); do
  printf 'hello world\n' > small.txt
  INACIRCLE_MEASURE=1 INACIRCLE_UNDO_WAIT_MS=0 \
    $R/inacircle run --notify markdown small.txt 2>&1 >/dev/null | grep measure
  rm -f small.md
done
# p50 and max of notify_cpu_us, self_cpu_us and vm_hwm over the 20 runs;
# repeat without --notify for the vm_hwm reference

stat -c %s $R/inacircle      # compare with 650472
```
