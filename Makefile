TARGET := $(shell rustc -vV | sed -n 's/^host: //p')

build:
	cargo build

build-release:
	cargo clean -p fish --release --target $(TARGET)
	RUSTFLAGS="-Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort" \
	cargo build --release \
	  -Z build-std=std \
	  -Z build-std-features= \
	  --target $(TARGET)

install: build-release
	sudo install -m 755 target/$(TARGET)/release/fish /usr/local/bin/fish
	@grep -q /usr/local/bin/fish /etc/shells || echo /usr/local/bin/fish | sudo tee -a /etc/shells
	chsh -s /usr/local/bin/fish

test:
	cargo test

# Build and test in release mode so CI doesn't duplicate work (debug + release).
test-ci:
	cargo test --release

# Usage: make bump-version [V=x.y.z]
# Without V, increments the patch version.
bump-version:
ifndef V
	$(eval OLD := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml))
	$(eval V := $(shell echo "$(OLD)" | awk -F. '{printf "%d.%d.%d", $$1, $$2, $$3+1}'))
endif
	sed -i '' 's/^version = ".*"/version = "$(V)"/' Cargo.toml
	cargo check --quiet 2>/dev/null
	git add Cargo.toml Cargo.lock
	git commit -m "bump version to $(V)"
	git tag "release/$(V)"

.PHONY: build build-release install test test-ci bump-version
