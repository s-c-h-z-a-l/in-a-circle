#!/usr/bin/env bash
# Measure the wheel's start cost on a second, untuned machine over SSH.
#
#   scripts/measure-remote.sh <ssh-host> [runs]
#
# Copies the local release inacircle and inacircle-text to a temporary dir on
# the host (nothing is built there), starts the wheel `runs` times inside the
# user's running niri session with INACIRCLE_MEASURE=1, closes it with Esc
# (wtype), and prints p50/max of the `measure wheel` lines plus machine facts.
# Method: docs/benchmarks/wheel.md. The host needs a logged-in, unlocked
# Wayland session and wtype. Every wait below has a deadline.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST=${1:?usage: measure-remote.sh <ssh-host> [runs]}
# 20 runs: the same count as the reference method (wheel.md, "Start cost").
RUNS=${2:-20}
# Session environment of the host's niri session. The host's user id 1000 and
# the first Wayland socket are what a single-user niri install uses.
REMOTE_RUNTIME=/run/user/1000
REMOTE_DISPLAY=wayland-1
# Wait between starting the wheel and sending Esc: the wheel commits after one
# compositor round trip (a few ms, wheel.md); 1 s leaves ample margin on a slow
# box and is still short. If it is too short the wheel is not mapped yet and
# the run reports no line (counted as a failure, never silently skipped).
SETTLE_S=1
# Hard per-run deadline for the wheel to exit after Esc. The wheel has its own
# deadlines; this one only protects the user's desktop if it hangs: 5 s is
# far above a normal close (ms) and short enough to never block the session.
EXIT_DEADLINE_S=5
# ssh-level guard so a dead host or a stuck remote command cannot hang us:
# a run takes SETTLE_S + up to EXIT_DEADLINE_S; the rest is connection setup.
SSH_RUN_TIMEOUT_S=$((SETTLE_S + EXIT_DEADLINE_S + 10))

SSH=(ssh -o BatchMode=yes -o ConnectTimeout=10)
SCP=(scp -q -o BatchMode=yes -o ConnectTimeout=10)

cargo build --release -q -p inacircle -p inacircle-text
R=$PWD/target/release
for b in inacircle inacircle-text; do [ -x "$R/$b" ] || { echo "missing $R/$b" >&2; exit 1; }; done

W=$(mktemp -d)
HELPER=$W/run-one.sh
# One run on the host. Isolated XDG state/cache/config dirs so the user's own
# state is untouched; the runtime dir and Wayland socket are the real ones.
cat > "$HELPER" <<'HELP'
#!/usr/bin/env bash
dir=$1; settle=$2; deadline=$3
export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1
[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] || { echo "ERROR no wayland socket"; exit 2; }
# A niri session with no connected monitor has no outputs: the wheel never maps
# and Esc has nothing to reach. Say so instead of timing out.
sock=$(ls "$XDG_RUNTIME_DIR"/niri.*.sock 2>/dev/null | head -n1)
if [ -n "$sock" ] && [ "$(NIRI_SOCKET=$sock niri msg --json outputs)" = "{}" ]; then
  echo "ERROR niri has no outputs (no monitor or dummy plug connected)"; exit 5
fi
export INACIRCLE_MEASURE=1 INACIRCLE_ACTIONS_DIR=$dir/actions
export XDG_CACHE_HOME=$dir/cache XDG_STATE_HOME=$dir/state XDG_CONFIG_HOME=$dir/config HOME=$dir/home
"$dir/inacircle" 2>"$dir/err" </dev/null &
pid=$!
sleep "$settle"
wtype -k Escape
for _ in $(seq $((deadline * 10))); do
  kill -0 "$pid" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$pid" 2>/dev/null; then
  kill -KILL "$pid"; wait "$pid" 2>/dev/null
  echo "ERROR wheel outlived the deadline"; sed "s/^/err: /" "$dir/err"; exit 3
fi
grep 'measure wheel' "$dir/err" || { echo "ERROR no measure line"; sed 's/^/err: /' "$dir/err"; exit 4; }
HELP

REMOTE=$("${SSH[@]}" "$HOST" mktemp -d)
cleanup() { "${SSH[@]}" "$HOST" rm -rf "$REMOTE" || true; rm -rf "$W"; }
trap cleanup EXIT

"${SSH[@]}" "$HOST" mkdir -p "$REMOTE/actions" "$REMOTE/cache" "$REMOTE/state" "$REMOTE/config" "$REMOTE/home"
"${SCP[@]}" "$R/inacircle" "$HOST:$REMOTE/"
"${SCP[@]}" "$R/inacircle-text" "$HOST:$REMOTE/actions/"
"${SCP[@]}" "$HELPER" "$HOST:$REMOTE/"
"${SSH[@]}" "$HOST" chmod +x "$REMOTE/inacircle" "$REMOTE/actions/inacircle-text" "$REMOTE/run-one.sh"

LINES=$W/lines
: > "$LINES"
fail=0
for i in $(seq "$RUNS"); do
  if out=$(timeout "$SSH_RUN_TIMEOUT_S" "${SSH[@]}" "$HOST" "$REMOTE/run-one.sh" "$REMOTE" "$SETTLE_S" "$EXIT_DEADLINE_S" 2>&1); then
    echo "$out" >> "$LINES"
  else
    echo "run $i failed: $out" >&2
    fail=$((fail + 1))
  fi
done

# field NAME -> sorted values; p50 = element ceil(n/2), max = last.
vals() { grep -o "$1=[0-9]*" "$LINES" | cut -d= -f2 | sort -n; }
p50() { vals "$1" | awk '{a[NR]=$1} END{if(NR) print a[int((NR+1)/2)]; else print "n/a"}'; }
mx() { vals "$1" | tail -n1; }

echo "== $(wc -l < "$LINES") of $RUNS runs measured ($fail failed) on $HOST"
for f in main_to_commit_cpu_us main_to_commit_wall_us; do
  echo "$f p50=$(p50 $f) max=$(mx $f)"
done
echo "pss_kb max=$(mx pss_kb)"
echo "idle_wakeups max=$(mx idle_wakeups)"
echo "== machine"
"${SSH[@]}" "$HOST" uname -r
"${SSH[@]}" "$HOST" grep -m1 "'model name'" /proc/cpuinfo
"${SSH[@]}" "$HOST" cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor || true
"${SSH[@]}" "$HOST" grep MemTotal /proc/meminfo
"${SSH[@]}" "$HOST" niri --version || true
echo "mitigated vulnerabilities: $("${SSH[@]}" "$HOST" "grep -l Mitigation /sys/devices/system/cpu/vulnerabilities/*" | wc -l)"
[ "$fail" -eq 0 ] || { echo "FAILED: $fail run(s)" >&2; exit 1; }
