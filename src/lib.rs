use byteorder::{BigEndian, ByteOrder};

use std::io::Result;
use std::net::SocketAddr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use log::*;

async fn copy<'a, T: AsyncRead + Unpin, U: AsyncWrite + Unpin>(sk1: &'a mut T, sk2: &'a mut U) {
    let mut buf = [0; 1024];
    loop {
        let len = sk1.read(&mut buf[..]).await;
        if let Err(err) = len {
            error!("copy: read error: {:?}", err);
            break;
        }

        let len = len.unwrap();
        if len == 0 {
            break;
        }

        match sk2.write_all(&buf[..len]).await {
            Err(err) => {
                error!("copy: write error: {:?}", err);
                break;
            }
            _ => {}
        }
    }
}

async fn handle(mut stream: TcpStream) -> Result<()> {
    let mut buf = [0; 1024];

    let len = stream.read(&mut buf).await?;

    if 1 + 1 + (buf[1] as usize) != len || buf[0] != b'\x05' {
        error!("invalid header");
        return Ok(());
    }
    stream.write_all(b"\x05\x00").await?;

    let len = stream.read(&mut buf).await?;
    if len <= 4 {
        error!("invalid proto");
        return Ok(());
    }

    let ver = buf[0];
    let cmd = buf[1];
    let atyp = buf[3];

    if ver != b'\x05' {
        error!("invalid proto");
        return Ok(());
    }

    if cmd != 1 {
        error!("Command not supported");
        stream
            .write_all(b"\x05\x07\x00\x01\x00\x00\x00\x00\x00\x00")
            .await?;
        return Ok(());
    }

    let addr;
    match atyp {
        1 => {
            if len != 10 {
                error!("invalid proto");
                return Ok(());
            }
            let dst_addr = IpAddr::V4(Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]));
            let dst_port = BigEndian::read_u16(&buf[8..]);
            addr = SocketAddr::new(dst_addr, dst_port).to_string();
        }
        3 => {
            let offset = 4 + 1 + (buf[4] as usize);
            if offset + 2 != len {
                error!("invalid proto");
                return Ok(());
            }
            let dst_port = BigEndian::read_u16(&buf[offset..]);
            let mut dst_addr = std::str::from_utf8(&buf[5..offset]).unwrap().to_string();
            dst_addr.push_str(":");
            dst_addr.push_str(&dst_port.to_string());
            addr = dst_addr;
        }
        4 => {
            if len != 22 {
                error!("invalid proto");
                return Ok(());
            }
            let dst_addr = IpAddr::V6(Ipv6Addr::new(
                ((buf[4] as u16) << 8) | buf[5] as u16,
                ((buf[6] as u16) << 8) | buf[7] as u16,
                ((buf[8] as u16) << 8) | buf[9] as u16,
                ((buf[10] as u16) << 8) | buf[11] as u16,
                ((buf[12] as u16) << 8) | buf[13] as u16,
                ((buf[14] as u16) << 8) | buf[15] as u16,
                ((buf[16] as u16) << 8) | buf[17] as u16,
                ((buf[18] as u16) << 8) | buf[19] as u16,
            ));
            let dst_port = BigEndian::read_u16(&buf[20..]);
            addr = SocketAddr::new(dst_addr, dst_port).to_string();
        }
        _ => {
            error!("Address type not supported, type={}", atyp);
            stream
                .write_all(b"\x05\x08\x00\x01\x00\x00\x00\x00\x00\x00")
                .await?;
            return Ok(());
        }
    }

    info!("incoming socket, request upstream: {:?}", addr);
    let up_stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => {
            error!("Upstream connect failed, {}", e);
            stream
                .write_all(b"\x05\x01\x00\x01\x00\x00\x00\x00\x00\x00")
                .await?;
            return Ok(());
        }
    };

    stream
        .write_all(b"\x05\x00\x00\x01\x00\x00\x00\x00\x00\x00")
        .await?;

    let (mut ri, mut wi) = stream.into_split();
    let (mut ro, mut wo) = up_stream.into_split();

    tokio::spawn(async move {
        copy(&mut ro, &mut wi).await;
    });

    copy(&mut ri, &mut wo).await;
    return Ok(());
}

pub async fn run_socks5(addr: SocketAddr) -> Result<()> {
    let listener: TcpListener = TcpListener::bind(&addr).await?;
    info!("Listening on: {}", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            handle(stream).await.unwrap();
        });
    }
}
