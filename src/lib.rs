use byteorder::{BigEndian, ByteOrder};

use std::io;
use std::io::Result;
use std::net::{IpAddr, Ipv4Addr};
use std::net::{SocketAddr, ToSocketAddrs};

use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;

use backtrace::Backtrace;

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio_executor::threadpool::blocking;

pub trait Encoder: Send + Sync {
    fn encode(&self, buf: &[u8]) -> &[u8];
    fn decode(&self, buf: &mut [u8], len: usize) -> Result<usize>;
    fn clone_box(&self) -> Box<dyn Encoder>;
}

impl Clone for Box<dyn Encoder> {
    fn clone(&self) -> Box<dyn Encoder> {
        self.clone_box()
    }
}

async fn read<'a, T: AsyncRead + Unpin>(
    s: &'a mut T,
    encoder: &Option<Box<dyn Encoder>>,
    buf: &'a mut [u8],
) -> Result<usize> {
    let sz = s.read(buf).await?;
    match encoder {
        Some(ref e) => e.decode(buf, sz),
        None => Ok(sz),
    }
}

async fn write_all<'a, T: AsyncWrite + Unpin>(
    s: &'a mut T,
    encoder: &Option<Box<dyn Encoder>>,
    buf: &'a [u8],
) -> Result<()> {
    let buf2 = match encoder {
        Some(ref e) => e.encode(buf),
        None => buf,
    };
    s.write_all(buf2).await?;
    Ok(())
}

async fn copy<'a, T: AsyncRead + Unpin, U: AsyncWrite + Unpin>(
    stream1: &'a mut T,
    stream2: &'a mut U,
    encoder: &Option<Box<dyn Encoder>>,
) -> io::Result<()> {
    let mut buf = [0; 1024];
    loop {
        let len = read(stream1, encoder, &mut buf).await?;
        if len == 0 {
            println!("socket broken");
            break;
        }
        write_all(stream2, encoder, &buf[..len]).await?
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
            let bt = Backtrace::new();
            println!("{:?}", bt);
            println!(
                "ToSockAddrsFuture thread id={:?}",
                std::thread::current().id()
            );

            println!("resolving {}", self.addr);
            let mut addrs_iter = self.addr.to_socket_addrs().unwrap();
            addrs_iter.next().unwrap()
        })
        .map_err(|err| panic!("Error in blocing block: {:?}", err))
    }
}

async fn handle(mut stream: TcpStream, encoder: Option<Box<dyn Encoder>>) -> Result<()> {
    let mut buf = [0; 1024];

    let len = read(&mut stream, &encoder, &mut buf).await?;

    if 1 + 1 + (buf[1] as usize) != len || buf[0] != b'\x05' {
        println!("invalid header");
        return Ok(());
    }
    write_all(&mut stream, &encoder, b"\x05\x00").await?;

    let len = read(&mut stream, &encoder, &mut buf).await?;
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
            &encoder,
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
            println!(
                "before ToSockAddrsFuture thread id={:?}",
                std::thread::current().id()
            );

            addr = ToSockAddrsFuture { addr: dst_addr }.await.unwrap();
        }
        _ => {
            println!("Address type not supported, type={}", atyp);
            write_all(
                &mut stream,
                &encoder,
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
                &encoder,
                b"\x05\x01\x00\x01\x00\x00\x00\x00\x00\x00",
            )
            .await?;
            return Ok(());
        }
    };

    write_all(
        &mut stream,
        &encoder,
        b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00",
    )
    .await?;
    println!("handle thread id={:?}", std::thread::current().id());

    let (mut ri, mut wi) = stream.split();
    let (mut ro, mut wo) = up_stream.split();

    let encoder1 = encoder.as_ref().map(|e| e.clone());
    tokio::spawn(async move {
        copy(&mut ri, &mut wo, &encoder1).await.unwrap();
    });

    let encoder2 = encoder.as_ref().map(|e| e.clone());
    tokio::spawn(async move {
        copy(&mut ro, &mut wi, &encoder2).await.unwrap();
    });

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
