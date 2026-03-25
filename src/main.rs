use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use log::{info, warn};
use std::{
    error::Error,
    net::{IpAddr, SocketAddr},
    panic,
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::{
        broadcast::{self, Sender as BroadcastSender, error::RecvError as BroadcastRecvError},
        oneshot::{self, Receiver as OneshotReceiver, Sender as OneshotSender},
    },
    task::{JoinError, JoinSet},
    time::sleep,
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

const SEND_TIMEOUT: Duration = Duration::from_secs(2);

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

const CAPACITY: usize = 32768;

const PORT: u16 = 3000;

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    simple_logger::init_with_env()?;
    let (abort_tx, abort_rx) = oneshot::channel();
    let (broadcast_tx, _) = broadcast::channel(CAPACITY);
    set_shutdown_hook(abort_tx, broadcast_tx.clone());
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::from([0, 0, 0, 0]), PORT)).await?;
    info!("listening on {PORT}");
    let mut join_set = JoinSet::new();
    serve(abort_rx, broadcast_tx, &listener, &mut join_set).await;
    shutdown(&mut join_set).await;
    Ok(())
}

fn set_shutdown_hook(abort_tx: OneshotSender<()>, broadcast_tx: BroadcastSender<Broadcast>) {
    let _ = ctrlc::set_handler({
        let mut once = Some((abort_tx, broadcast_tx));
        move || {
            if let Some((abort_tx, broadcast_tx)) = once.take() {
                let _ = broadcast_tx.send(Broadcast {
                    addr: None,
                    message: Message::Close(None),
                });
                let _ = abort_tx.send(());
            }
        }
    });
}

async fn serve(
    mut abort_rx: OneshotReceiver<()>,
    broadcast_tx: BroadcastSender<Broadcast>,
    listener: &TcpListener,
    join_set: &mut JoinSet<()>,
) {
    loop {
        tokio::select! {
            _ = &mut abort_rx => {
                break;
            }
            accept_result = listener.accept() => {
                while let Some(join_result) = join_set.try_join_next() {
                    rejoin(join_result);
                }
                match accept_result {
                    Ok((tcp_stream, addr)) => {
                        join_set.spawn(accept(broadcast_tx.clone(), addr, tcp_stream));
                    }
                    Err(error) => {
                        warn!("accept error: {error}");
                    }
                }
            },
        }
    }
}

async fn shutdown(join_set: &mut JoinSet<()>) {
    if !join_set.is_empty() {
        info!("waiting for {} tasks before shutdown", join_set.len());
    }
    while let Some(join_result) = join_set.join_next().await {
        rejoin(join_result);
    }
}

fn rejoin(result: Result<(), JoinError>) {
    if let Err(error) = result
        && let Ok(panic) = error.try_into_panic()
    {
        panic::resume_unwind(panic);
    }
}

async fn accept(broadcast_tx: BroadcastSender<Broadcast>, addr: SocketAddr, tcp_stream: TcpStream) {
    info!("new connection: {addr}");
    if let Some(ws_stream) = handshake(addr, tcp_stream).await {
        listen(broadcast_tx, addr, ws_stream).await;
    }
}

async fn handshake(addr: SocketAddr, tcp_stream: TcpStream) -> Option<WebSocketStream<TcpStream>> {
    select! {
        ws_result = tokio_tungstenite::accept_async(tcp_stream) => {
             match ws_result {
                Ok(ws_stream) => Some(ws_stream),
                Err(error) => {
                    warn!("handshake error with {addr}: {error}");
                    None
                }
            }
        },
        _ = sleep(HANDSHAKE_TIMEOUT) => {
            warn!("handshake timeout with {addr}");
            None
        }
    }
}

async fn listen(
    broadcast_tx: BroadcastSender<Broadcast>,
    addr: SocketAddr,
    ws_stream: WebSocketStream<TcpStream>,
) {
    let mut broadcast_rx = broadcast_tx.subscribe();
    let (mut ws_sink, mut ws_source) = ws_stream.split();
    loop {
        select! {
            maybe_message = recv(addr, &mut ws_source) => {
                match maybe_message {
                    Some(message) if matches!(&message, Message::Close(_)) => {}
                    Some(message) => {
                        if broadcast_tx.send(Broadcast { addr: Some(addr), message }).is_err() {
                            warn!("broadcast overload");
                        }
                    }
                    None => {
                        break;
                    }
                }
            }
            broadcast_result = broadcast_rx.recv() => {
                match broadcast_result {
                    Ok(broadcast) => {
                        if broadcast.addr.is_none_or(|broadcast_addr| broadcast_addr != addr) {
                            send(broadcast.message, addr, &mut ws_sink).await;
                        }
                    }
                    Err(BroadcastRecvError::Lagged(_)) => {},
                    Err(BroadcastRecvError::Closed) => {
                        break;
                    }
                }
            }
        }
    }
}

async fn recv(
    addr: SocketAddr,
    source: &mut SplitStream<WebSocketStream<TcpStream>>,
) -> Option<Message> {
    loop {
        select! {
            maybe_message_result = source.next() => {
                let Some(message_result) = maybe_message_result else {
                    info!("closed {addr}");
                    break None;
                };
                match message_result {
                    Ok(message) if matches!(&message, Message::Close(_)) => {}
                    Ok(message) => {
                        break Some(message);
                    }
                    Err(error) => {
                        warn!("recv error with {addr}: {error}");
                    }
                }
            }
            _ = sleep(IDLE_TIMEOUT) => {
                warn!("idle timeout with {addr}");
                break None;
            }
        }
    }
}

async fn send(
    message: Message,
    addr: SocketAddr,
    ws_sink: &mut SplitSink<WebSocketStream<TcpStream>, Message>,
) {
    select! {
        send_result = ws_sink.send(message) => {
            if let Err(error) = send_result {
                warn!("send error with {addr}: {error}");
            }
        },
        _ = sleep(SEND_TIMEOUT) => {
            warn!("send timeout with {addr}");
        }
    };
}

#[derive(Debug, Clone)]
struct Broadcast {
    addr: Option<SocketAddr>,
    message: Message,
}
