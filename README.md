# tokio-socks5
socks5 server using tokio-rs async/await, nightly rust

# Build and Run

```
cargo run --example main 127.0.0.1:8080
```

# lib usage

```rust
    let rt = tokio::runtime::Builder::new().build().unwrap();
    rt.block_on(run_socks5(addr, None)).unwrap();
```
