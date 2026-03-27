use clap::Parser;
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use log::{debug, error, info, warn};
use std::{
    error::Error,
    net::{IpAddr, SocketAddr},
    panic,
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    select, signal,
    sync::broadcast::{Receiver, Sender, channel, error::RecvError},
    task::{JoinError, JoinSet},
    time::timeout,
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

#[derive(Debug, Clone, Copy, Parser)]
struct Args {
    /// The addr to listen to
    #[arg(short = 'd', long, default_value_t = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), 8881))]
    addr: SocketAddr,
    /// Buffer capacity
    #[arg(short, long, default_value_t = 32768)]
    capacity: usize,
    /// Handshake timeout in seconds
    #[arg(long, default_value_t = 10)]
    handshake_timeout: u64,
    /// Send timeout in seconds
    #[arg(long, default_value_t = 15)]
    send_timeout: u64,
    /// Idle timeout in seconds
    #[arg(long, default_value_t = 30)]
    idle_timeout: u64,
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    let _ = simple_logger::init_with_env();
    let args = Args::parse();
    let listener = TcpListener::bind(args.addr).await?;
    info!("listening on {addr}", addr = args.addr);
    let (tx, _) = channel(args.capacity);
    let mut join_set = JoinSet::new();
    loop {
        select! {
            _ = interruption() => {
                break;
            }
            accept_result = listener.accept() => {
                clear_ready_tasks(&mut join_set);
                match accept_result {
                    Ok((tcp_stream, addr)) => {
                        let accept_future = accept(args, tx.clone(), addr, tcp_stream);
                        join_set.spawn(accept_future);
                    }
                    Err(error) => {
                        error!("accept error: {error}");
                    }
                }
            }
        }
    }
    if !join_set.is_empty() {
        info!("waiting for {} tasks before shutdown", join_set.len());
        let _ = tx.send(Broadcast::Interruption);
        clear_tasks(&mut join_set).await;
    }
    Ok(())
}

fn clear_ready_tasks(join_set: &mut JoinSet<()>) {
    while let Some(join_result) = join_set.try_join_next() {
        rejoin(join_result);
    }
}

async fn clear_tasks(join_set: &mut JoinSet<()>) {
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

#[cfg(unix)]
async fn interruption() -> Result<(), Box<dyn Error>> {
    let mut hangup = signal::unix::signal(signal::unix::SignalKind::hangup())?;
    let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    select! {
        _ = signal::ctrl_c() => {},
        _ = hangup.recv() => {},
        _ = terminate.recv() => {},
    }
    Ok(())
}

#[cfg(not(unix))]
async fn interruption() -> Result<(), Box<dyn Error>> {
    signal::ctrl_c().await;
    Ok(())
}

async fn accept(args: Args, tx: Sender<Broadcast>, addr: SocketAddr, tcp_stream: TcpStream) {
    info!("new connection: {addr}");
    if let Some(ws_stream) = handshake(args, addr, tcp_stream).await {
        listen(args, addr, tx, ws_stream).await;
    }
}

async fn handshake(
    args: Args,
    addr: SocketAddr,
    tcp_stream: TcpStream,
) -> Option<WebSocketStream<TcpStream>> {
    let handshake_result = timeout(
        Duration::from_secs(args.handshake_timeout),
        tokio_tungstenite::accept_async(tcp_stream),
    )
    .await;
    match handshake_result {
        Ok(Ok(ws_stream)) => {
            debug!("handshake success with {addr}");
            Some(ws_stream)
        }
        Ok(Err(error)) => {
            error!("handshake error with {addr}: {error}");
            None
        }
        Err(_) => {
            warn!("handshake timeout with {addr}");
            None
        }
    }
}

async fn listen(
    args: Args,
    addr: SocketAddr,
    mut tx: Sender<Broadcast>,
    ws_stream: WebSocketStream<TcpStream>,
) {
    let mut rx = tx.subscribe();
    let (mut ws_sink, mut ws_source) = ws_stream.split();
    loop {
        let must_abort = select! {
            must_abort = broadcast(args,addr, &mut tx, &mut ws_source) => must_abort,
            must_abort = relay(args, addr, &mut rx, &mut ws_sink) => must_abort,
        };
        if must_abort {
            break;
        }
    }
}

async fn broadcast(
    args: Args,
    addr: SocketAddr,
    tx: &mut Sender<Broadcast>,
    ws_source: &mut SplitStream<WebSocketStream<TcpStream>>,
) -> bool {
    let maybe_message = recv(args, addr, ws_source).await;
    match maybe_message {
        Some(message) if matches!(&message, Message::Close(_)) => false,
        Some(message) => {
            let _ = tx.send(Broadcast::Echo { addr, message });
            false
        }
        None => true,
    }
}

async fn recv(
    args: Args,
    addr: SocketAddr,
    ws_source: &mut SplitStream<WebSocketStream<TcpStream>>,
) -> Option<Message> {
    loop {
        let recv_result = timeout(Duration::from_secs(args.idle_timeout), ws_source.next()).await;
        break match recv_result {
            Ok(Some(Ok(message))) if matches!(&message, Message::Close(_)) => {
                continue;
            }
            Ok(Some(Ok(message))) => Some(message),
            Ok(Some(Err(error))) => {
                error!("recv error with {addr}: {error}");
                continue;
            }
            Ok(None) => {
                info!("closed {addr}");
                None
            }
            Err(_) => {
                warn!("idle timeout with {addr}");
                None
            }
        };
    }
}

async fn relay(
    args: Args,
    addr: SocketAddr,
    rx: &mut Receiver<Broadcast>,
    ws_sink: &mut SplitSink<WebSocketStream<TcpStream>, Message>,
) -> bool {
    let broadcast_result = rx.recv().await;
    match broadcast_result {
        Ok(Broadcast::Echo {
            addr: broadcast_addr,
            message,
        }) => {
            if addr != broadcast_addr {
                send(args, addr, message, ws_sink).await;
            }
            false
        }
        Ok(Broadcast::Interruption) => {
            send(args, addr, Message::Close(None), ws_sink).await;
            true
        }
        Err(RecvError::Lagged(_)) => false,
        Err(RecvError::Closed) => true,
    }
}

async fn send(
    args: Args,
    addr: SocketAddr,
    message: Message,
    ws_sink: &mut SplitSink<WebSocketStream<TcpStream>, Message>,
) {
    let send_result = timeout(
        Duration::from_secs(args.send_timeout),
        ws_sink.send(message),
    )
    .await;
    match send_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            error!("send error with {addr}: {error}");
        }
        Err(_) => {
            warn!("send timeout with {addr}");
        }
    }
}

#[derive(Debug, Clone)]
enum Broadcast {
    Echo { addr: SocketAddr, message: Message },
    Interruption,
}
