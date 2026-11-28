# Roadmap

The maintainer's backlog, in order: each item is done as a plan in `docs/plans/` and one PR. Agents change it only when the person asks.

- [ ] **The machine.** lodka's own VMM on Hypervisor.framework boots the guest's kernel: guest memory, vCPUs with PSCI, the in-kernel GICv3 and the virtual timer, a device tree, PL011 and PL031; the kernel's config and its build. Plan: [20261126-machine.md](plans/completed/20261126-machine.md).
- [ ] **Virtio and the guest's disks.** Virtio over MMIO with the console, entropy and block devices; the guest's erofs root and `lodka-guest` as PID 1 with the ext4 data disk at `/var/lib`, sparse, with discard. Plan: [20261126-disks.md](plans/completed/20261126-disks.md).
- [ ] **Docker over vsock.** The network on vmnet and the vsock device; the control channel, DHCP and dockerd in the guest, Docker's socket on the Mac over vsock, a clean power-off. Plan: [20261126-docker.md](plans/completed/20261126-docker.md).
- [ ] **The commands.** `lodka start|stop|status|ssh|delete`: the VM in a process of its own under a lock, its settings, sshd with lodka's key, containers' ports on localhost, a reboot in place. Plan: [20261126-commands.md](plans/completed/20261126-commands.md).
- [ ] **The Mac's folders in the guest, fast.** `/Users` at the same path: FUSE requests from the guest kernel served by lodka from macOS files (inodes by dev/ino and `fsgetpath`, `getattrlistbulk`, `clonefile`), long entry and attribute timeouts with FSEvents invalidating what changed, requests on the vCPU's thread through a hypercall instead of a virtqueue round trip. Measured against OrbStack and virtiofs on the same Mac. Plan: [20261127-folders.md](plans/completed/20261127-folders.md).
- [ ] **x86_64 images through Rosetta.** The Rosetta runtime shared into the guest and registered with binfmt_misc.
- [ ] **Memory back to the Mac.** Free page reporting (virtio-balloon), so an idle VM gives back what it no longer uses. Plan: [20261127-memory.md](plans/completed/20261127-memory.md).
- [ ] **A Docker context.** `lodka start` adds a `lodka` context and `lodka stop` leaves it, so `docker context use lodka` works without `DOCKER_HOST`.
- [ ] **Releases.** A tarball with the binary, `Image` and `rootfs.img`, and a Homebrew formula.

Not planned: x86 Macs, Intel guests emulated, Kubernetes, a GUI.
