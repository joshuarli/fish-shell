install:
    cargo build --release
    sudo install -m 755 target/release/fish /usr/local/bin/fish
    @grep -q /usr/local/bin/fish /etc/shells || echo /usr/local/bin/fish | sudo tee -a /etc/shells
    chsh -s /usr/local/bin/fish
