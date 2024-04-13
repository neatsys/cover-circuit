use std::{io::ErrorKind, net::SocketAddr};

use bincode::Options;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
};
use tracing::{warn, Instrument};

use crate::{event::SendEvent, net::Buf};

use super::{Incoming, Protocol, MAX_BUF_LEN};

pub struct Tcp(bytes::Bytes);

type TcpPreamble = Option<SocketAddr>;

const TCP_PREAMBLE_LEN: usize = 16;

impl Tcp {
    pub fn new(addr: impl Into<Option<SocketAddr>>) -> anyhow::Result<Self> {
        let addr = addr.into();
        let mut preamble = bincode::options().serialize(&addr)?;
        assert!(preamble.len() < TCP_PREAMBLE_LEN);
        preamble.resize(TCP_PREAMBLE_LEN, Default::default());
        Ok(Self(preamble.into()))
    }

    async fn read_task(
        mut stream: OwnedReadHalf,
        mut on_buf: impl FnMut(&[u8]) -> anyhow::Result<()>,
        remote: impl Into<Option<SocketAddr>>,
    ) {
        let remote = remote.into();
        if let Err(err) = async {
            loop {
                let len = match stream.read_u64().await {
                    Ok(len) => len as _,
                    Err(err) if matches!(err.kind(), ErrorKind::UnexpectedEof) => break Ok(()),
                    Err(err) => Err(err)?,
                };
                anyhow::ensure!(len <= MAX_BUF_LEN, "invalid buffer length {len}");
                let mut buf = vec![0; len];
                stream.read_exact(&mut buf).await?;
                on_buf(&buf)?
            }
        }
        .await
        {
            warn!(
                "{:?} (remote {remote:?}) >>> {:?} {err}",
                stream.peer_addr(),
                stream.local_addr()
            );
        }
    }

    async fn write_task<B: Buf>(
        mut stream: OwnedWriteHalf,
        mut receiver: UnboundedReceiver<B>,
        remote: SocketAddr,
    ) {
        while let Some(buf) = receiver.recv().await {
            if let Err(err) = async {
                stream.write_u64(buf.as_ref().len() as _).await?;
                stream.write_all(buf.as_ref()).await?;
                stream.flush().await
            }
            .await
            {
                warn!(
                    "{:?} >=> {:?} (remote {remote}) {err}",
                    stream.local_addr(),
                    stream.peer_addr()
                );
                break;
            }
        }
    }
}

impl<B: Buf> Protocol<B> for Tcp {
    type Sender = UnboundedSender<B>;

    fn connect(
        &self,
        remote: SocketAddr,
        on_buf: impl FnMut(&[u8]) -> anyhow::Result<()> + Send + 'static,
    ) -> Self::Sender {
        let preamble = self.0.clone();
        let (sender, receiver) = unbounded_channel();
        tokio::spawn(async move {
            let task = async {
                let mut stream = TcpStream::connect(remote).await?;
                stream.set_nodelay(true)?;
                stream.write_all(&preamble).await?;
                anyhow::Result::<_>::Ok(stream)
            };
            let stream = match task
                .instrument(tracing::debug_span!("connect", ?remote))
                .await
            {
                Ok(stream) => stream,
                Err(err) => {
                    warn!(">=> {remote} {err}");
                    return;
                }
            };
            let (read, write) = stream.into_split();
            tokio::spawn(Self::read_task(read, on_buf, remote));
            tokio::spawn(Self::write_task(write, receiver, remote));
        });
        sender
    }

    type Incoming = (TcpPreamble, TcpStream);

    fn accept(
        (preamble, stream): Self::Incoming,
        on_buf: impl FnMut(&[u8]) -> anyhow::Result<()> + Send + 'static,
    ) -> Option<(SocketAddr, Self::Sender)> {
        let (read, write) = stream.into_split();
        tokio::spawn(Tcp::read_task(read, on_buf, preamble));
        if let Some(remote) = preamble {
            let (sender, receiver) = unbounded_channel();
            tokio::spawn(Tcp::write_task(write, receiver, remote));
            Some((remote, sender))
        } else {
            // write.forget()
            None
        }
    }
}

pub async fn accept_session(
    listener: TcpListener,
    mut sender: impl SendEvent<Incoming<(TcpPreamble, TcpStream)>>,
) -> anyhow::Result<()> {
    loop {
        let (mut stream, peer_addr) = listener.accept().await?;
        let task = async {
            stream.set_nodelay(true)?;
            let mut preamble = vec![0; TCP_PREAMBLE_LEN];
            stream.read_exact(&mut preamble).await?;
            anyhow::Result::<_>::Ok(
                bincode::options()
                    .allow_trailing_bytes()
                    .deserialize(&preamble)?,
            )
        };
        let preamble = match task.await {
            Ok(preamble) => preamble,
            Err(err) => {
                warn!("{peer_addr} {err}");
                continue;
            }
        };
        // println!("{peer_addr} -> {remote}");
        sender.send(Incoming((preamble, stream)))?
    }
}

// `simplex::Tcp` provides a stateless `impl SendMessage` which initiate an
// ephemeral tcp connection for every message. this results in a setup closer to
// udp, but the performance will be much worse, and it cannot accept incoming
// connections anymore. you can use a second `TcpControl` that wrapped as
//   Inline(&mut control, &mut UnreachableTimer)
// to `impl SendEvent<Incoming>` and pass into `tcp_accept_session` for incoming
// connections. check unreplicated benchmark for demonstration. the simplex
// variant is compatible with the default duplex one i.e. it is ok to have
// messages sent by simplex tcp net to be received with a duplex one, and vice
// versa
pub mod simplex {
    use std::net::SocketAddr;

    use bincode::Options;
    use tokio::io::AsyncWriteExt;
    use tracing::warn;

    use crate::net::{Buf, IterAddr, SendMessage};

    use super::{TcpPreamble, TCP_PREAMBLE_LEN};

    pub struct Tcp;

    impl<B: Buf> SendMessage<SocketAddr, B> for Tcp {
        fn send(&mut self, dest: SocketAddr, message: B) -> anyhow::Result<()> {
            tokio::spawn(async move {
                if let Err(err) = async {
                    // have to enable REUSEADDR otherwise port number exhausted after sending to
                    // same `dest` 28K messages within 1min
                    // let mut stream = TcpStream::connect(dest).await?;
                    let socket = tokio::net::TcpSocket::new_v4()?;
                    socket.set_reuseaddr(true)?;
                    let mut stream = socket.connect(dest).await?;
                    let mut preamble = bincode::options().serialize(&TcpPreamble::None)?;
                    preamble.resize(TCP_PREAMBLE_LEN, Default::default());
                    stream.write_all(&preamble).await?;
                    stream.write_u64(message.as_ref().len() as _).await?;
                    stream.write_all(message.as_ref()).await?;
                    stream.flush().await?;
                    anyhow::Result::<_>::Ok(())
                }
                .await
                {
                    warn!("simplex >>> {dest} {err}")
                }
            });
            Ok(())
        }
    }

    impl<B: Buf> SendMessage<IterAddr<'_, SocketAddr>, B> for Tcp {
        fn send(&mut self, dest: IterAddr<'_, SocketAddr>, message: B) -> anyhow::Result<()> {
            for addr in dest.0 {
                self.send(addr, message.clone())?
            }
            Ok(())
        }
    }
}

// cSpell:words quic bincode rustls libp2p kademlia oneshot rcgen unreplicated
// cSpell:words neatworks
// cSpell:ignore nodelay reuseaddr
