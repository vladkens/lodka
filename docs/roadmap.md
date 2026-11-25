# Roadmap

The maintainer's backlog, in order: each item is done as a plan in `docs/plans/` and one PR. Agents change it only when the person asks.

- [ ] **Our own VMM with Docker.** `lodka start|stop|status|ssh|delete`: the VM on Hypervisor.framework with virtio over MMIO (console, entropy, disks, network on vmnet, vsock), the guest kernel and erofs root, `lodka-guest` as PID 1 with DHCP, sshd and dockerd, Docker's socket over vsock, ports on localhost. Plan: [20261125-vmm.md](plans/completed/20261125-vmm.md).
- [ ] **The Mac's folders in the guest, fast.** `/Users` at the same path: FUSE requests from the guest kernel served by lodka from macOS files (inodes by dev/ino and `fsgetpath`, `getattrlistbulk`, `clonefile`), long entry and attribute timeouts with FSEvents invalidating what changed, requests on the vCPU's thread through a hypercall instead of a virtqueue round trip. Measured against OrbStack and virtiofs on the same Mac.
- [ ] **x86_64 images through Rosetta.** The Rosetta runtime shared into the guest and registered with binfmt_misc.
- [ ] **Memory back to the Mac.** Free page reporting (virtio-balloon), so an idle VM gives back what it no longer uses.
- [ ] **A Docker context.** `lodka start` adds a `lodka` context and `lodka stop` leaves it, so `docker context use lodka` works without `DOCKER_HOST`.
- [ ] **Releases.** A tarball with the binary, `Image` and `rootfs.img`, and a Homebrew formula.

Not planned: x86 Macs, Intel guests emulated, Kubernetes, a GUI.
