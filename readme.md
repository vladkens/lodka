# lodka

**A small, fast Docker VM for Apple silicon Macs.**

lodka runs Docker in a small Linux VM, started from the terminal as Colima is, and aims to be as
fast as OrbStack: its own VMM on Hypervisor.framework, no QEMU, no Virtualization.framework.

- One binary in Rust; the VM runs in the background in a process of its own.
- A small guest: a kernel with only what Docker needs, a read-only erofs root, Docker's data on a
  sparse disk that gives freed space back to the Mac.
- Containers' ports on `localhost`; the VM's own address (new each start) reachable too.
- The Mac's `/Users` in the VM at the same path: `docker run -v "$PWD:/app"` just works.
- Memory the VM no longer uses goes back to the Mac, its idle file cache too.
- `lodka ssh` into the VM as root, with a key lodka makes.

Requires a Mac with Apple silicon on macOS 26 or later.

## Install

```sh
git clone https://github.com/vladkens/lodka && cd lodka
make build images    # the guest is built in a container: OrbStack or Colima running
cp target/release/lodka /usr/local/bin/ && mkdir -p /usr/local/share/lodka
cp guest/out/Image guest/out/rootfs.img /usr/local/share/lodka/
```

lodka signs itself ad hoc with the hypervisor and virtualization entitlements the first time it
starts the VM.

## Use

```sh
lodka start                        # all the Mac's CPUs, half its memory up to 8 GiB, 64 GiB disk
lodka start --cpus 4 --memory 6    # sizes are kept for the next start
docker context use lodka           # the context lodka start adds
docker run --rm -p 8080:80 nginx   # http://localhost:8080
lodka ssh                          # a root shell in the VM; lodka ssh -- uname -a
lodka status
lodka stop
lodka delete                       # the disk, with all images, containers and volumes
```

## How it works

- The VMM: Hypervisor.framework's vCPUs and its in-kernel GICv3; virtio over MMIO for the console,
  entropy, the two disks, the network, vsock, the Mac's folders and the balloon; PSCI for the vCPUs
  and power; the Linux `Image` booted directly with a device tree, no firmware.
- The Mac's folders: virtio-fs, served from macOS files on the vCPU's own thread. Edits on the Mac
  reach the guest's caches and inotify in containers.
- Memory: guest RAM in 2 MiB pieces, then 64 KiB once given back; the guest reports the memory it
  frees (free page reporting), and the VMM gives whole pieces back to macOS at once. Idle, the
  guest trims its page cache.
- The network: a vmnet shared network, so macOS does NAT, DHCP and DNS. Packets go between vmnet
  and guest memory with no copy.
- Docker's socket and containers' ports on the Mac: each connection goes over vsock into the
  guest, to dockerd's socket or the port on the guest's loopback.
- In the guest, `lodka-guest` is PID 1: it mounts the file systems, starts DHCP, sshd and dockerd
  and keeps them running, and tells lodka the guest's address and listening ports.

Files, by the XDG Base Directory spec: settings in `~/.config/lodka/lodka.conf`, the disk and SSH
key in `~/.local/share/lodka`, logs (`lodka.log`, `console.log`) and the Docker socket in
`~/.local/state/lodka`.

## License

MIT
