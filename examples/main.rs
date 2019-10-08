use std::env;
use std::net::SocketAddr;
use tokio_socks5::run_socks5;

fn main() {
    let addr = env::args()
        .nth(1)
        .unwrap_or("127.0.0.1:20002".to_string())
        .parse::<SocketAddr>()
        .unwrap();

    let rt = tokio::runtime::Builder::new().build().unwrap();
    rt.block_on(run_socks5(addr, None)).unwrap();
}
