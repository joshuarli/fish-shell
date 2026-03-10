target := arch() + "-apple-darwin"
nightly := "nightly-2026-02-23"

build:
    cargo build

build-release:
    cargo clean -p fish --release --target {{ target }}
    RUSTFLAGS="-Zlocation-detail=none -Zunstable-options -Cpanic=immediate-abort" \
    cargo +{{ nightly }} build --release \
      -Z build-std=std \
      -Z build-std-features= \
      --target {{ target }}

install: build-release
    sudo install -m 755 target/{{ target }}/release/fish /usr/local/bin/fish
    @grep -q /usr/local/bin/fish /etc/shells || echo /usr/local/bin/fish | sudo tee -a /etc/shells
    chsh -s /usr/local/bin/fish
