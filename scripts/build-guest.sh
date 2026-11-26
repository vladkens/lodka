#!/bin/sh
# Builds the guest into guest/out/: `kernel` (Image), `rootfs` (rootfs.img, erofs) or `all`.
# Runs itself in an Alpine container on a Docker that mounts the Mac's folders (OrbStack, Colima;
# not lodka yet); `rootfs` needs the guest binary built first (`make guest`). Kernel sources stay
# in the `lodka-build` volume.
set -eu
KVER=${KVER:-6.18.65}
ALPINE=${ALPINE:-3.24}
IMAGE=${IMAGE:-mirror.gcr.io/library/alpine:$ALPINE}
GUEST_BIN=target/aarch64-unknown-linux-musl/release/lodka-guest

if [ "${1:-}" != --inside ]; then
  what=${1:-all}
  case $what in kernel | rootfs | all) ;; *) echo "usage: $0 kernel|rootfs|all" >&2 && exit 2 ;; esac
  cd "$(dirname "$0")/.."
  [ "$what" = kernel ] || [ -x "$GUEST_BIN" ] || { echo "$GUEST_BIN missing: run make guest" >&2 && exit 1; }
  mkdir -p guest/out
  exec docker run --rm --platform linux/arm64 -v "$PWD:/src" -v lodka-build:/build \
    -e KVER="$KVER" -e GUEST_BIN="$GUEST_BIN" "$IMAGE" sh /src/scripts/build-guest.sh --inside "$what"
fi

what=$2
apk add -q --no-cache build-base linux-headers bc flex bison perl openssl-dev elfutils-dev xz \
  diffutils findutils erofs-utils curl

kernel() {
  cd /build
  [ -f "linux-$KVER/Makefile" ] || curl -fsSL "https://cdn.kernel.org/pub/linux/kernel/v${KVER%%.*}.x/linux-$KVER.tar.xz" | tar xJ
  cd "linux-$KVER"
  make -s allnoconfig KCONFIG_ALLCONFIG=/src/guest/kernel.config
  missing=0
  for line in $(grep -E '^CONFIG_[A-Z0-9_]+=y' /src/guest/kernel.config); do
    grep -qx "$line" .config || { echo "not applied: $line" && missing=1; }
  done
  [ $missing = 0 ] || { echo "kernel.config: some options need a dependency" >&2 && exit 1; }
  make -s -j"$(nproc)" Image
  cp arch/arm64/boot/Image /src/guest/out/Image
  cp .config /src/guest/out/Image.config
  ls -l /src/guest/out/Image
}

rootfs() {
  root=/tmp/rootfs
  rm -rf $root && mkdir -p $root/etc/apk
  cp -a /etc/apk/keys /etc/apk/repositories $root/etc/apk/
  # docker-engine brings containerd, runc and tini; busybox has udhcpc, ip and mount; git is
  # for BuildKit's Git sources (`docker build <repo>.git`)
  apk add -q --root $root --initdb --no-cache alpine-baselayout busybox musl ca-certificates \
    docker-engine openssh-server openssh-keygen e2fsprogs git iptables ip6tables
  rm -rf $root/var/cache/apk $root/etc/apk
  cp -a /src/guest/rootfs/. $root/
  install -s -m 755 "/src/$GUEST_BIN" $root/sbin/lodka-guest
  # Mount points: the Mac's folders, Rosetta; /var is a tmpfs with the data disk at /var/lib
  mkdir -p $root/Users $root/Volumes $root/private $root/mnt/rosetta
  rm -rf $root/var/* $root/var/.[!.]*
  mkfs.erofs -zlz4hc,12 -T0 --all-root /src/guest/out/rootfs.img $root
  ls -l /src/guest/out/rootfs.img
}

case $what in
  kernel) kernel ;;
  rootfs) rootfs ;;
  all) kernel; rootfs ;;
esac
