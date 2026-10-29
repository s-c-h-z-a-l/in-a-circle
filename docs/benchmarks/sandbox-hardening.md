# Sandbox and data-safety hardening

What the hardening branch changed in how actions are confined and how
outputs are published and undone, what evidence exists, and what it does
**not** claim. The contract is in [architecture.md](../architecture.md)
(Sandbox, Output policy, Jobs and history); numbers for limits are in
[resource-limits.md](../resource-limits.md); timing and memory are in
[hardening-performance.md](hardening-performance.md). The threat model is
a manifest or action that is hostile native code, run as the user.

## What is protected, and what is not

Protected, for the started program and every descendant, from before the
manifest is read: files nobody named (Landlock), the inputs (read only),
the network and local IPC (no socket of any family can be created, no
inherited descriptor survives), the parent's secrets (clean environment),
and other processes (no ptrace, `process_vm_*`, pidfd calls or signals
outside the sandbox). An action reads its inputs and writes its own work
folder; that is not confidentiality for content it was handed on purpose.

Not claimed:

- Perfect isolation. A same-user attacker who can modify the engine, its
  private state or the user's files is outside the model. So are a kernel
  bug, a privileged attacker and hardware side channels.
- Resource containment of a hostile process tree. `RLIMIT_DATA` is per
  process and there is no cgroup, memory quota or `RLIMIT_NPROC`; the group
  kill bounds how long a tree lives, not what it takes meanwhile
  ([resource-limits.md](../resource-limits.md#not-covered)).
- Hiding file names: path lookup and `stat` outside the sandbox still work.
- Compatibility with every converter. The policy denies by name; the
  shipped actions run, converters from third parties were not tried
  (ioctl default-deny, scheduler and affinity setters, System V IPC and
  POSIX queues, `F_ADD_SEALS`).

## Mandatory, and how a refusal works

Nothing runs without the whole sandbox, built-in actions and manifest
probes included. Before a start the gate checks, by trying, not by kernel
version: Landlock ABI >= 6 (the signal and abstract-socket scopes), and a
parent that is not root, has one uid and gid for real, effective, saved and
filesystem, and holds no permitted, effective or ambient capability. An
inheritable capability alone is accepted (systemd gives it to every
session; with `no_new_privs` it cannot become a real capability). Nothing
is dropped on the user's behalf.

If the gate fails, every input fails with `sandbox unavailable: <why>`, the
history records `sandbox=no`, the notification reads `Sandbox unavailable.`
and a manifest probe makes its binary unusable for that run. The wheel and
the engine stay usable; only actions are refused. Inside the child, any
failing setup step (Landlock restrict, seccomp install, closing
descriptors) aborts the start; an architecture other than x86_64 or aarch64
has no policy and runs nothing. There is no warning-only mode and no
environment switch that weakens it. `INACIRCLE_REQUIRE_LANDLOCK=1`,
`INACIRCLE_REQUIRE_KERNEL_CONTROLS=1` and `INACIRCLE_REQUIRE_PRIVILEGED_TEST=1`
only make the test suite fail instead of skipping where a control cannot be
proved.

## Boundaries, by kind

| Boundary | How | Not covered |
|---|---|---|
| ABI floor | Landlock >= 6, handling every right the kernel knows; TCP (ABI 4), UDP (ABI 10), pathname unix connect (ABI 9) added where present | Below 9 pathname unix sockets are not Landlock's, below 10 UDP on inherited sockets: seccomp (no socket creation) and closed descriptors cover both. Real kernels with ABI 6 or 7 were not run; the floor is exercised with injected ABI values (5 refused, 6 accepted). |
| Architecture | One seccomp table, two native syscall-number sets, built at compile time into a static BPF program per architecture. i386 and AArch32 calls kill the process; x32 gets `EPERM` | Only x86_64 and aarch64 |
| IPC | `socket` and `socketpair` denied for every family; abstract sockets and signals scoped by Landlock | A later network action needs its own capability design, not an exception |
| Environment | Cleared; only `PATH=/usr/bin:/bin`, `LC_ALL`/`LANG=C.UTF-8`, and a private `TMPDIR` (action: `<work>/tmp`, 0700; manifest: a path that does not exist). No bus, display, agent, token or loader variable | |
| Descriptors | stdin `/dev/null`; stdout and stderr are the engine's pipes; every other descriptor from 3 up is made close-on-exec before `execve` | |
| Process tree | Own session and process group; `setsid`, `setpgid`, namespaces, `ptrace`, `process_vm_*`, pidfd calls, `kcmp`, the parent-death, subreaper and ptracer prctls, io_uring, bpf, perf, userfaultfd, mounts and inotify/fanotify denied; `SIGKILL` of the group always goes out before the leader is reaped; the engine is the child subreaper, so the kernel's child list (not `/proc`, which `hidepid` can blind) says whether anything is left; the leader gets `PR_SET_PDEATHSIG` (`SIGKILL`) before the sandbox | A member killed earlier and never waited for counts as left behind: a false positive that fails the input, by design. If the supervisor is `SIGKILL`ed, the leader dies with it, its descendants do not (no cgroup) |
| Privilege | `no_new_privs`; the gate above | |

Tests of escape attempts run under the same policy as the product. Where
the host already refuses `unshare` to an unprivileged user (AppArmor,
`max_user_namespaces`, or a PID namespace without a user namespace), the
`EPERM` is the host's; the policy's own denial of namespaces is covered by
the table tests, and a positive control (`setsid` is allowed without the
filter) guards the group test.

## Data safety: publication and Undo

**The counterexample.** Plan 7's Undo compared the output's recorded
metadata and then removed it by path. A process that held a directory
descriptor could put another object in place after the last check; the
`unlinkat` then removed the replacement while a lease on the original was
still valid. This was reproduced on Btrfs and on ext4 in an ARM guest, with
own fixtures only (uid 1000, no capabilities). A check followed by an
`unlink` cannot be made safe, so the deleting Undo was withdrawn.

**What replaced it.**

- Every input gets its own output folder (a single output too), built
  privately: files are copied into fresh inodes of the engine's own in a
  private 0700 staging folder on the target filesystem, validated, given
  their final modes, synced, and recorded; the folder is published by one
  `renameat2(RENAME_NOREPLACE)`. A filesystem without it is refused. An
  error or cancel before that point removes only the engine's own staging;
  after it nothing is removed. Other inputs' published results stay.
- A synced history block (the group's identity and every child's baseline,
  with the planned place) is written **before** the rename. If it cannot be
  written, the group is not published.
- Undo never deletes. It moves each input's group into the freedesktop.org
  Trash by one no-replace `rename` on the same filesystem (home Trash, else
  the volume's), after a synced recovery intent. A changed, missing or extra
  child, an unsafe Trash, a remote filesystem, `EXDEV` or any doubt keeps
  the whole group where it is. A group moved and then found different is
  moved back; if its way back is taken it stays in the Trash and the result
  says where.
- The result is per group: `Undone` / `Moved to Trash`, `Partially undone`
  with counts, `Nothing undone`, `Undo uncertain`, `Undo incomplete`.

**Limits of the keep contract.**

- Identity is metadata (device, inode, size, mtime, ctime, type, owner,
  mode, link count, child count), **not** content: there is no digest. A
  content change that preserves all of it is not seen.
- A descriptor, mapping or directory handle that someone holds can still
  write after the move; no revocation or byte-level guarantee is made.
- A reported place is proved at one moment (the known path, or the path
  resolved from the held descriptor, at most 4 tries); a later rename by
  another program is not followed. If it cannot be proved the result says
  "unknown place" with an anchor, never a guess.
- Where no descriptor is held across a check and a removal (a registry
  entry, a step's work folder), device and inode cannot prove identity
  against inode reuse; exploiting it needs the same user, removes at worst
  one of our own bookkeeping files or an unpublished folder, and never a
  published output.
- Power loss: durability is promised only for what was synced, on
  filesystems that honour it. The undo tests ran on ext4, Btrfs and tmpfs;
  other filesystems are untested.

**Storage cost.** A Trashed group still occupies its disk space; nothing
is emptied or scanned by In.A.Circle. During a publication the work
folder's files and the engine's fresh copies coexist briefly (up to one more
copy of the group's outputs), by design; this was not measured. Kept
outputs stay where they are.

**External trash rules apply.** The group is an ordinary Trash entry. The
desktop's or the user's own rules (automatic emptying after some days,
a size quota, `trash-empty`, a file manager's "Empty Trash") apply to it
and may remove it for good; In.A.Circle neither prevents nor undoes that,
and does not interpret them. Restoring from the file manager restores the
whole output folder. The manual check on Nautilus and Dolphin is listed as
open below.

## Evidence and status

| Environment | What ran | Status |
|---|---|---|
| Reference desktop (x86_64, kernel 7.2.9-cachyos, Landlock ABI 10) | Full suite with `INACIRCLE_REQUIRE_LANDLOCK=1`; last recorded run 1150 passed, 0 failed, 3 ignored; qualification | Re-run for the closing verification |
| Native CI, x86_64 and aarch64 (Ubuntu 26.04, kernel 7.0.0-1017-azure, ABI 8) | Qualification, hardening tests with required kernel controls, musl hardening, privileged test, fuzz smoke, release memory guard, size gate | Green on `c84f4ad` (run 40835264396) |
| Own aarch64 guest (kernel 7.0.0-38-generic, ABI 8, TCG) | Hardening tests 31/31 on `b696450` | Functional only; emulated; not re-run on the final commit |
| ThinkCentre M910q (kernel 7.2.11-arch1-1, ABI 10) | Qualification; the performance series | See [hardening-performance.md](hardening-performance.md) |
| ThinkPad X280 (Plasma) | Earlier qualification only | OPEN: machine unavailable, no Plasma run |
| Manual niri check (conversions, hover, Esc, Undo, Trash restore in Nautilus) | Written checklist | OPEN: not yet run by hand on the final candidate |
| Manual Plasma and Dolphin check | | OPEN: no Plasma machine |

Time-limited mutation runs and fuzz targets were run on the changed code;
surviving mutants are individually justified (equivalent, or needing fault
injection such as a failing `fsync`) in the maintainer's working notes. Landlock ABI 6
and 7 kernels, filesystems other than ext4, Btrfs and tmpfs, physical ARM
hardware, a power cut and third-party converters are **untested**.
