.PHONY: prepare check test build update clean

LINT := uv run --script https://raw.githubusercontent.com/vladkens/lint/main/rust.py
CARGO_FLAGS := --release --locked
GUEST := --target aarch64-unknown-linux-musl -p lodka-guest

prepare:
	cargo +nightly fmt
	$(LINT) fix
	cargo clippy $(CARGO_FLAGS) --fix --all-targets --allow-dirty --allow-no-vcs -- -D warnings

check:
	cargo +nightly fmt --check
	cargo clippy $(CARGO_FLAGS) --all-targets -- -D warnings
	cargo clippy $(CARGO_FLAGS) $(GUEST) -- -D warnings
	$(LINT)

test:
	cargo test $(CARGO_FLAGS)

build:
	cargo build $(CARGO_FLAGS)
	ls -lh target/release/lodka

update:
	cargo upgrade -i
	cargo update

clean:
	cargo clean

.PHONY: guest kernel rootfs images dist

# The guest's PID 1, static for Linux on arm64
guest:
	cargo build $(CARGO_FLAGS) $(GUEST)

# guest/out/Image and guest/out/rootfs.img, built in a container on OrbStack or Colima
kernel:
	scripts/build-guest.sh kernel

rootfs: guest
	scripts/build-guest.sh rootfs

images: kernel rootfs

# The release archive for CI's rust-build: the binary, the guest (make images) and the readme
dist:
	cargo build $(CARGO_FLAGS) -p lodka
	rm -rf target/distrib && mkdir -p target/distrib
	tar -czf target/distrib/lodka-dev-aarch64-apple-darwin.tar.gz -C $(CURDIR)/target/release lodka \
		-C $(CURDIR)/guest/out Image rootfs.img -C $(CURDIR) readme.md LICENSE
	tar -tvzf target/distrib/lodka-dev-aarch64-apple-darwin.tar.gz
