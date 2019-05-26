#![feature(await_macro, async_await)]

use byteorder::{BigEndian, ByteOrder};

use std::io;
use std::io::Result;
use std::net::{IpAddr, Ipv4Addr};
use std::net::{Shutdown, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};

use tokio::async_wait;
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;
use tokio_threadpool::blocking;

pub trait Encoder {
    fn encode(&mut self, buf: &[u8]) -> &[u8];
    fn decode(&mut self, buf: &mut [u8], len: usize) -> Result<usize>;
    fn clone(&self) -> Box<Encoder + Send + Sync>;
}

struct MyTcpStream {
    stream: Arc<Mutex<TcpStream>>,
    encoder: Option<Box<Encoder + Send + Sync>>,
}

impl Read for MyTcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = self.stream.lock().unwrap().read(buf)?;
        match self.encoder {
            Some(ref mut encoder) => encoder.decode(buf, len),
            None => Ok(len),
        }
    }
}

impl Write for MyTcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut buf2 = buf;
        if let Some(ref mut encoder) = self.encoder {
            buf2 = encoder.encode(buf);
        }
        self.stream.lock().unwrap().write(buf2)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsyncRead for MyTcpStream {}

impl AsyncWrite for MyTcpStream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.stream.lock().unwrap().shutdown(Shutdown::Write)?;
        Ok(().into())
    }
}

impl MyTcpStream {
    fn clone(&mut self) -> MyTcpStream {
        MyTcpStream {
            stream: self.stream.clone(),
            encoder: match self.encoder {
                Some(ref mut encoder) => Some(encoder.clone()),
                None => None,
            },
        }
    }

    fn new(stream: TcpStream) -> Self {
        MyTcpStream {
            stream: Arc::new(Mutex::new(stream)),
            encoder: None,
        }
    }

    fn with_encoder(stream: TcpStream, encoder: Option<Box<Encoder + Send + Sync>>) -> Self {
        MyTcpStream {
            stream: Arc::new(Mutex::new(stream)),
            encoder: encoder,
        }
    }
}

async fn copy_stream<'a>(
    stream1: &'a mut MyTcpStream,
    stream2: &'a mut MyTcpStream,
) -> io::Result<()> {
    let mut buf = [0; 1024];
    loop {
        let len = async_wait!(stream1.read_async(&mut buf))?;
        if len == 0 {
            println!("socket broken");
            break;
        }
        async_wait!(stream2.write_all_async(&buf[..len]))?
    }
    Ok(())
}

struct ToSockAddrsFuture {
    addr: String,
}

impl Future for ToSockAddrsFuture {
    type Item = SocketAddr;
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        blocking(|| {
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

async fn handle(mut stream: MyTcpStream) -> Result<()> {
    let mut buf = [0; 1024];

    let len = async_wait!(stream.read_async(&mut buf))?;

    if 1 + 1 + (buf[1] as usize) != len || buf[0] != b'\x05' {
        println!("invalid header");
        return Ok(());
    }
    async_wait!(stream.write_all_async(b"\x05\x00"))?;

    let len = async_wait!(stream.read_async(&mut buf))?;
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
        async_wait!(stream.write_all_async(b"\x05\x07\x00\x01\x00\x00\x00\x00\x00\x00"))?;
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

            addr = async_wait!(ToSockAddrsFuture { addr: dst_addr }).unwrap();
        }
        _ => {
            println!("Address type not supported, type={}", atyp);
            async_wait!(stream.write_all_async(b"\x05\x08\x00\x01\x00\x00\x00\x00\x00\x00"))?;
            return Ok(());
        }
    }

    println!("incoming socket, request upstream: {:?}", addr);
    let up_stream = match async_wait!(TcpStream::connect(&addr)) {
        Ok(s) => s,
        Err(e) => {
            println!("Upstream connect failed, {}", e);
            async_wait!(stream.write_all_async(b"\x05\x01\x00\x01\x00\x00\x00\x00\x00\x00"))?;
            return Ok(());
        }
    };

    async_wait!(stream.write_all_async(b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00"))?;
    println!("handle thread id={:?}", std::thread::current().id());

    let mut stream2 = stream.clone();
    let mut up_stream = MyTcpStream::new(up_stream);
    let mut up_stream2 = up_stream.clone();
    tokio::spawn_async(async move {
        println!("upstream: copy thread id={:?}", std::thread::current().id());
        if let Err(err) = async_wait!(copy_stream(&mut up_stream2, &mut stream2)) {
            println!("{}", err);
        }
        let _ = up_stream2.shutdown();
        let _ = stream2.shutdown();
        println!(
            "upstream: exit copy thread id={:?}",
            std::thread::current().id()
        );
    });
    tokio::spawn_async(async move {
        println!(
            "downstream: copy thread id={:?}",
            std::thread::current().id()
        );
        if let Err(err) = async_wait!(copy_stream(&mut stream, &mut up_stream)) {
            println!("{}", err);
        }
        let _ = stream.shutdown();
        let _ = up_stream.shutdown();
        println!(
            "downstream: exit copy thread id={:?}",
            std::thread::current().id()
        );
    });

    return Ok(());
}

pub async fn run_socks5(addr: SocketAddr, encoder: Option<Box<Encoder + Send + Sync>>) {
    let listener = TcpListener::bind(&addr).unwrap();
    println!("Listening on: {}", addr);
    let mut incoming = listener.incoming();

    while let Some(stream) = async_wait!(incoming.next()) {
        let stream = stream.unwrap();
        let encoder = match encoder {
            Some(ref tmp) => Some(tmp.as_ref().clone()),
            None => None,
        };
        let stream2 = MyTcpStream::with_encoder(stream, encoder);
        tokio::spawn_async(async move {
            if let Err(err) = async_wait!(handle(stream2)) {
                println!("{}", err);
            }
        })
    }
}
