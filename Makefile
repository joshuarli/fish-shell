TARGET := $(shell rustc -vV | sed -n 's/^host: //p')
NIGHTLY := nightly-2026-02-23

build:
	cargo build

build-release:
	cargo clean -p fish --release --target $(TARGET)
	RUSTFLAGS="-Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort" \
	cargo +$(NIGHTLY) build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)

install: build-release
	sudo install -m 755 target/$(TARGET)/release/fish /usr/local/bin/fish
	@grep -q /usr/local/bin/fish /etc/shells || echo /usr/local/bin/fish | sudo tee -a /etc/shells
	chsh -s /usr/local/bin/fish

.PHONY: build build-release install
