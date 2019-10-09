use byteorder::{BigEndian, ByteOrder};

use std::io;
use std::io::Result;
use std::net::{IpAddr, Ipv4Addr};
use std::net::{SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;
use tokio_executor::threadpool::blocking;

pub trait Encoder: Send + Sync {
    fn encode(&mut self, buf: &[u8]) -> &[u8];
    fn decode(&mut self, buf: &mut [u8], len: usize) -> Result<usize>;
    fn clone_box(&self) -> Box<dyn Encoder>;
}

impl Clone for Box<dyn Encoder> {
    fn clone(&self) -> Box<dyn Encoder> {
        self.clone_box()
    }
}

async fn read<'a, T: AsyncRead + Unpin>(
    s: &'a mut T,
    encoder: &mut Option<Box<dyn Encoder>>,
    buf: &'a mut [u8],
) -> Result<usize> {
    loop {
        let sz = s.read(buf).await?;

        if sz == 0 {
            return Ok(sz);
        }
        match encoder {
            Some(ref mut e) => {
                let res = e.decode(buf, sz);
                match res {
                    Ok(sz2) if sz2 == 0 => continue,
                    _ => return res,
                }
            }
            None => return Ok(sz),
        }
    }
}

async fn write_all<'a, T: AsyncWrite + Unpin>(
    s: &'a mut T,
    encoder: &mut Option<Box<dyn Encoder>>,
    buf: &'a [u8],
) -> Result<()> {
    let buf2 = match encoder {
        Some(ref mut e) => e.encode(buf),
        None => buf,
    };
    s.write_all(buf2).await?;
    Ok(())
}

async fn copy<'a, T: AsyncRead + Unpin, U: AsyncWrite + Unpin>(
    stream1: &'a mut T,
    stream2: &'a mut U,
    encoder1: &mut Option<Box<dyn Encoder>>,
    encoder2: &mut Option<Box<dyn Encoder>>,
) -> io::Result<()> {
    let mut buf = [0; 1024];
    loop {
        let len = read(stream1, encoder1, &mut buf).await?;
        if len == 0 {
            println!("socket broken");
            break;
        }
        write_all(stream2, encoder2, &buf[..len]).await?
    }
    Ok(())
}

struct ToSockAddrsFuture {
    addr: String,
}

impl Future for ToSockAddrsFuture {
    type Output = io::Result<SocketAddr>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        blocking(|| {
            println!("resolving {}", self.addr);
            let mut addrs_iter = self.addr.to_socket_addrs().unwrap();
            addrs_iter.next().unwrap()
        })
        .map_err(|err| panic!("Error in blocing block: {:?}", err))
    }
}

async fn handle(mut stream: TcpStream, mut encoder: Option<Box<dyn Encoder>>) -> Result<()> {
    let mut buf = [0; 1024];

    let len = read(&mut stream, &mut encoder, &mut buf).await?;

    if 1 + 1 + (buf[1] as usize) != len || buf[0] != b'\x05' {
        println!("invalid header");
        return Ok(());
    }
    write_all(&mut stream, &mut encoder, b"\x05\x00").await?;

    let len = read(&mut stream, &mut encoder, &mut buf).await?;
    if len <= 4 {
        println!("invalid proto");
        return Ok(());
    }

    let ver = buf[0];
    let cmd = buf[1];
    let atyp = buf[3];

    if ver != b'\x05' {
        println!("invalid proto");
        return Ok(());
    }

    if cmd != 1 {
        println!("Command not supported");
        write_all(
            &mut stream,
            &mut encoder,
            b"\x05\x07\x00\x01\x00\x00\x00\x00\x00\x00",
        )
        .await?;
        return Ok(());
    }

    let addr: SocketAddr;
    match atyp {
        1 => {
            if len != 10 {
                println!("invalid proto");
                return Ok(());
            }
            let dst_addr = IpAddr::V4(Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]));
            let dst_port = BigEndian::read_u16(&buf[8..]);
            addr = SocketAddr::new(dst_addr, dst_port);
        }
        3 => {
            let offset = 4 + 1 + (buf[4] as usize);
            if offset + 2 != len {
                println!("invalid proto");
                return Ok(());
            }
            let dst_port = BigEndian::read_u16(&buf[offset..]);
            let mut dst_addr = std::str::from_utf8(&buf[5..offset]).unwrap().to_string();
            dst_addr.push_str(":");
            dst_addr.push_str(&dst_port.to_string());
            addr = ToSockAddrsFuture { addr: dst_addr }.await.unwrap();
        }
        _ => {
            println!("Address type not supported, type={}", atyp);
            write_all(
                &mut stream,
                &mut encoder,
                b"\x05\x08\x00\x01\x00\x00\x00\x00\x00\x00",
            )
            .await?;
            return Ok(());
        }
    }

    println!("incoming socket, request upstream: {:?}", addr);
    let up_stream = match TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(e) => {
            println!("Upstream connect failed, {}", e);
            write_all(
                &mut stream,
                &mut encoder,
                b"\x05\x01\x00\x01\x00\x00\x00\x00\x00\x00",
            )
            .await?;
            return Ok(());
        }
    };

    write_all(
        &mut stream,
        &mut encoder,
        b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00",
    )
    .await?;

    let (mut ri, mut wi) = stream.split();
    let (mut ro, mut wo) = up_stream.split();

    let mut encoder1 = encoder.as_ref().map(|e| e.clone());
    let mut encoder3 = None;
    tokio::spawn(async move {
        copy(&mut ri, &mut wo, &mut encoder1, &mut encoder3)
            .await
            .unwrap();
    });

    let mut encoder2 = None;
    let mut encoder4 = encoder.as_ref().map(|e| e.clone());
    copy(&mut ro, &mut wi, &mut encoder2, &mut encoder4)
        .await
        .unwrap();

    return Ok(());
}

pub async fn run_socks5(addr: SocketAddr, encoder: Option<Box<dyn Encoder>>) -> Result<()> {
    let mut listener = TcpListener::bind(&addr).await?;
    println!("Listening on: {}", addr);

    loop {
        let encoder2 = encoder.as_ref().map(|e| e.clone());
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            handle(stream, encoder2).await.unwrap();
        });
    }
}
