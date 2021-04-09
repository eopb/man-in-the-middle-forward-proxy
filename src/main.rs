use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use std::{
    error::Error,
    fmt::{self, Display, Write},
    io::ErrorKind,
    net::SocketAddr,
};
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::{TcpListener, TcpStream},
};
use tokio_util::codec::{Decoder, Encoder};
use tracing::{debug, error, info, instrument, trace, Level};
use tracing_subscriber::fmt::format::FmtSpan;

static BIND_ADD: &str = "0.0.0.0:8080";
const BUFFER_SIZE: usize = 16 * 1024;

/// Destination address for request coming from client
struct Destination(String);

#[tokio::main]
async fn main() -> io::Result<()> {
    // Setting up logging
    {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::INFO)
            .with_span_events(FmtSpan::CLOSE)
            .finish();

        tracing::subscriber::set_global_default(subscriber)
            .expect("no global subscriber has been set");
    }

    info!("Listening at proxy address {}", BIND_ADD);
    let client = TcpListener::bind(BIND_ADD).await?;

    loop {
        let client = client.accept().await;

        if let Ok((stream, socket)) = client {
            info!("Received inbound connection on socket: {}", socket);
            // Must spawn task for handling each request
            tokio::spawn(async move { tunnel(stream).await });
        } else {
            error!("Failed to connect")
        }
    }
}

/// Handle each CONNECT request
#[instrument(level = "trace")]
async fn tunnel(client: TcpStream) -> io::Result<()> {
    // let (mut write, mut read) = ConnectMethod.framed(client).split();
    let mut client = ConnectMethod.framed(client);
    // Resolve DNS
    let destination = {
        let destination = client
            .next()
            .await
            .and_then(Result::ok)
            .ok_or(ErrorKind::ConnectionAborted)?
            .0;
        debug!("Resolving DNS {}", destination);
        let destination_addr: Vec<SocketAddr> =
            tokio::net::lookup_host(destination).await?.collect();
        info!("Resolved DNS to {:?}", destination_addr);
        if destination_addr.is_empty() {
            error!("Cannot resolve DNS");
            return Err(io::Error::from(ErrorKind::AddrNotAvailable));
        }
        destination_addr[0] // is the zero index really what we need here?
    };

    let server = TcpStream::connect(destination).await?;
    client.send(()).await?;

    stich_client_server(client.into_inner(), server).await;
    Ok(())
}

/// Encoder and Decoder for Connect requests
#[derive(Debug)]
struct ConnectMethod;

impl Decoder for ConnectMethod {
    type Item = Destination;
    type Error = HttpConnectError;

    /// Decode CONNECT request to find the destination.
    #[instrument(level = "trace", err)]
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut headers);
        let _res = req.parse(src)?;

        req.method.map_or(Err(Self::Error::NoMethod), |method| {
            if method == "CONNECT" {
                req.path.map_or(Err(Self::Error::MissingPath), |path| {
                    Ok(Some(Destination(path.to_owned())))
                })
            } else {
                Err(Self::Error::WrongMethod(method.to_owned()))
            }
        })
    }
}

impl Encoder<()> for ConnectMethod {
    type Error = io::Error;

    /// Accept connect request
    #[instrument(level = "trace", err)]
    fn encode(&mut self, item: (), dst: &mut BytesMut) -> Result<(), Self::Error> {
        // The response for an accepted CONNECT request
        dst.write_str("HTTP/1.1 200 OK\r\n\r\n")
            .map_err(|_| io::Error::from(io::ErrorKind::Other))
    }
}

/// Spawn tasks gluing the proxy together.
pub async fn stich_client_server(client: TcpStream, server: TcpStream) {
    let (client_recv, client_send) = io::split(client);
    let (server_recv, server_send) = io::split(server);

    tokio::spawn(async move { swap_foo_with_bar(client_recv, server_send).await });
    tokio::spawn(async move { swap_foo_with_bar(server_recv, client_send).await });
}

// Pass bytes through the proxy while replacing instances of foo with bar
pub async fn swap_foo_with_bar(
    mut source: ReadHalf<TcpStream>,
    mut dest: WriteHalf<TcpStream>,
) -> io::Result<()> {
    fn replace_bytes(buf: &mut [u8], from: &[u8], to: &[u8]) {
        for i in 0..=buf.len() - from.len() {
            if buf[i..].starts_with(from) {
                buf[i..(i + from.len())].clone_from_slice(to);
            }
        }
    }
    let mut buffer = [0; BUFFER_SIZE];

    loop {
        let read_result = source.read(&mut buffer).await;

        let n = match read_result {
            Ok(n) if n == 0 => {
                break;
            }
            Ok(n) => n,
            Err(_) => {
                break;
            }
        };

        replace_bytes(&mut buffer[..n], b"foo", b"bar");

        trace!("{}", String::from_utf8_lossy(&buffer[..n])); // Hoping this line will not affect performance when not running in trace mode

        if dest.write_all(&buffer[..n]).await.is_err() {
            break;
        }
    }

    dest.shutdown().await?;

    Ok(())
}

#[derive(Debug)]
enum HttpConnectError {
    WrongMethod(String),
    NoMethod,
    MissingPath,
    InvalidHttp(httparse::Error),
    IoError(io::Error),
}

impl From<io::Error> for HttpConnectError {
    fn from(error: std::io::Error) -> Self {
        Self::IoError(error)
    }
}

impl From<httparse::Error> for HttpConnectError {
    fn from(error: httparse::Error) -> Self {
        Self::InvalidHttp(error)
    }
}

impl Display for HttpConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for HttpConnectError {}
