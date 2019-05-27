# tokio-socks5
socks5 server using tokio-rs async/await, nightly rust

# Build and Run

```
cargo run --example main 127.0.0.1:8080
```

# lib usage

```rust
    let mut rt = tokio::runtime::Builder::new().build().unwrap();
    // implement encoder if custom encode/decode is needed
    rt.block_on_async(run_socks5(addr, None));
```
