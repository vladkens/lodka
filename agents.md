# lodka

A small, fast Docker VM for Apple silicon Macs, run from the terminal as Colima is, as fast as
OrbStack: its own VMM on Hypervisor.framework boots a small Linux with Docker and SSH; the Mac
reaches it over a vmnet network, Docker's socket over vsock. Agents carry out each change end to
end by following this file.

Each fact has one source, which this file points to: what lodka does is `readme.md` and
`lodka help`, dependencies `Cargo.toml`, checks the `Makefile`, the guest's kernel
`guest/kernel.config`, its packages `scripts/build-guest.sh`, what comes next `docs/roadmap.md`,
the work under way `docs/plans/`.

## Scope

- Host: macOS 26+ on Apple silicon (Hypervisor.framework's in-kernel GICv3 needs macOS 15, a
  vmnet network without root needs 26). Guests are arm64 only.
- Our own VMM, not Virtualization.framework, in a crate of its own (`crates/lodka-vmm`) that
  knows nothing of lodka's commands, files or settings: one process per VM (`lodka _vm`), one
  thread per vCPU, virtio devices over MMIO, each on a thread of its own, moving data straight
  between guest memory and the host (`preadv`/`pwritev`, `vmnet_read`/`vmnet_write`).
- The guest: an `allnoconfig` kernel with what Docker needs built in, no modules; an erofs root
  from Alpine's packages, read-only; an ext4 data disk at `/var/lib`; `lodka-guest` as PID 1.
  Built in a container (`make images`), hosted by no one.
- Pure Rust: Hypervisor.framework, vmnet and libdispatch through `extern "C"` declared by hand,
  blocks through `block2`; no Swift, no C compiled, no bindings generated.
- Prefer a small own routine to a dependency. Change dependencies only with `cargo add` /
  `cargo remove` / `cargo upgrade`, never by hand.
- OrbStack may be taken apart to learn how it does a thing; its code is never copied.

## Code

- Start every `.rs` file with the line
  `// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka`, then a `//!` doc
  whose first sentence says what the file does.
- Comment only what the code can't say: platform quirks, measured numbers, ordering that matters,
  the spec a layout comes from. Write code that reads like the code around it. Code taken from
  another project names it in the file's header (project, licence, link to the file).
- Every `unsafe` block has a `// SAFETY:` line.
- Helper scripts go in `scripts/`, kebab-case.

## Tests

- `make check` (format; clippy with warnings denied for the host and the guest; rust-lint's
  rules) and `make test` run before every commit.
- Unit-test what breaks silently: the device tree, virtqueue chains, the vsock header, the
  control lines, the arguments and settings.
- The proof of a change to the VMM, a device or the guest is the VM on the test Mac (`ssh m2`):
  started, `docker run` and `lodka ssh` working, the change's own check, `lodka stop` clean.
  Measure what is meant to be fast against OrbStack on the same Mac.
- A run on the test Mac sets `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_STATE_HOME` to folders
  under `/private/tmp`, and leaves OrbStack, Colima, their containers, volumes, Docker contexts and
  `~/.docker` as they were.

## Workflow

- `docs/roadmap.md` is the person's backlog: change it only when the person asks. Each item is
  done as a plan and one PR.
- Keep multi-step work as a checklist in `docs/plans/yyyymmdd-<name>.md`, only ticking it during
  the work; when done, add `Done: yyyy-mm-dd` on top and a short `## Result`, and move it to
  `docs/plans/completed/`.
- Before committing, run `make check` and `make test`. `make prepare` rewrites files: not a check.
- Make each commit one meaningful change a person can check: one short lowercase line, no body,
  docs updated in the same commit. Push only when asked.
- Ask the person only when what lodka does must change, a heavy dependency is needed, or the
  request is ambiguous. Otherwise decide and say so in your report.
- Keep docs to `readme.md` (users), the rustdoc, `agents.md`, the roadmap and the plans. No
  progress journals, run logs or disclaimer paragraphs.
