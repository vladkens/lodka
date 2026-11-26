#!/bin/sh
# Builds the guest into guest/out/: `kernel` (Image). Runs itself in an Alpine container on a
# Docker that mounts the Mac's folders (OrbStack, Colima; not lodka yet). Kernel sources stay in
# the `lodka-build` volume.
set -eu
KVER=${KVER:-6.18.65}
ALPINE=${ALPINE:-3.24}
IMAGE=${IMAGE:-mirror.gcr.io/library/alpine:$ALPINE}

if [ "${1:-}" != --inside ]; then
  what=${1:-kernel}
  case $what in kernel) ;; *) echo "usage: $0 kernel" >&2 && exit 2 ;; esac
  cd "$(dirname "$0")/.."
  mkdir -p guest/out
  exec docker run --rm --platform linux/arm64 -v "$PWD:/src" -v lodka-build:/build \
    -e KVER="$KVER" "$IMAGE" sh /src/scripts/build-guest.sh --inside "$what"
fi

what=$2
apk add -q --no-cache build-base linux-headers bc flex bison perl openssl-dev elfutils-dev xz \
  diffutils findutils curl

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

case $what in
  kernel) kernel ;;
esac
