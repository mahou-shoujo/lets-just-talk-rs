use clap::Parser;
use futures_util::{
    SinkExt, StreamExt,
    future::select,
    stream::{SplitSink, SplitStream, iter},
};
use log::{debug, error, info, warn};
use std::{
    collections::{HashMap, VecDeque},
    error::Error,
    fmt::{self, Display},
    net::{IpAddr, SocketAddr},
    panic,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    select, signal,
    sync::{
        RwLock,
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    },
    task::{JoinError, JoinSet},
    time::timeout,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
        http::Uri,
    },
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Parser)]
struct Arguments {
    /// The addr to listen to
    #[arg(short = 'd', long, default_value_t = SocketAddr::new(IpAddr::from([127, 0, 0, 1]), 8881))]
    addr: SocketAddr,
    /// Handshake timeout in seconds
    #[arg(long, default_value_t = 10)]
    handshake_timeout: u64,
    /// Send timeout in seconds
    #[arg(long, default_value_t = 15)]
    send_timeout: u64,
    /// Idle timeout in seconds
    #[arg(long, default_value_t = 30)]
    idle_timeout: u64,
    /// Buffer size in bytes per client
    #[arg(long, default_value_t = 57600)]
    buffer_size: u64,
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    let _ = simple_logger::init_with_env();
    let arguments = Arguments::parse();
    let state = State::default();
    let mut join_set = JoinSet::new();
    let listener = TcpListener::bind(arguments.addr).await?;
    info!("listening on {addr}", addr = arguments.addr);
    loop {
        select! {
            _ = interruption() => {
                break;
            }
            accept_result = listener.accept() => {
                clear_ready_tasks(&mut join_set);
                match accept_result {
                    Ok((tcp_stream, socket_addr)) => {
                        let client = Client {
                            arguments,
                            state: state.clone(),
                            client_id: ClientId { socket_addr, uuid: Uuid::new_v4() },
                            room_id: RoomId { uri: None },
                        };
                        let fut = accept(client, tcp_stream);
                        join_set.spawn(fut);
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
        state.interrupt().await;
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

async fn accept(mut client: Client, tcp_stream: TcpStream) {
    info!("{client}: new connection");
    if let Some(ws_stream) = handshake(&mut client, tcp_stream).await {
        listen(&client, ws_stream).await;
    }
}

async fn handshake(
    client: &mut Client,
    tcp_stream: TcpStream,
) -> Option<WebSocketStream<TcpStream>> {
    let handshake_fut = tokio_tungstenite::accept_hdr_async(
        tcp_stream,
        #[expect(clippy::result_large_err)]
        |req: &Request, res: Response| {
            client.room_id.uri = Some(req.uri().clone());
            Ok(res)
        },
    );
    let handshake_fut_with_timeout = timeout(
        Duration::from_secs(client.arguments.handshake_timeout),
        handshake_fut,
    );
    let handshake_result = handshake_fut_with_timeout.await;
    match handshake_result {
        Ok(Ok(ws_stream)) => {
            debug!("{client}: handshake success");
            Some(ws_stream)
        }
        Ok(Err(error)) => {
            error!("{client}: handshake error: {error}");
            None
        }
        Err(_) => {
            warn!("{client}: handshake timeout");
            None
        }
    }
}

async fn listen(client: &Client, ws_stream: WebSocketStream<TcpStream>) {
    let (mut ws_sink, mut ws_source) = ws_stream.split();
    let (tx, mut rx) = unbounded_channel();
    let mut queue = VecDeque::new();
    client.register(tx).await;
    select(
        Box::pin(broadcast(client, &mut ws_source)),
        Box::pin(relay(client, &mut rx, &mut queue, &mut ws_sink)),
    )
    .await;
    client.unregister().await;
}

async fn broadcast(client: &Client, ws_source: &mut SplitStream<WebSocketStream<TcpStream>>) {
    while let Some(message) = recv(client, ws_source).await {
        client.broadcast(message).await;
    }
}

async fn recv(
    client: &Client,
    ws_source: &mut SplitStream<WebSocketStream<TcpStream>>,
) -> Option<Message> {
    loop {
        let recv_result = timeout(
            Duration::from_secs(client.arguments.idle_timeout),
            ws_source.next(),
        )
        .await;
        break match recv_result {
            Ok(Some(Ok(message))) if matches!(&message, Message::Close(_)) => {
                continue;
            }
            Ok(Some(Ok(message))) => Some(message),
            Ok(Some(Err(error))) => {
                error!("{client}: recv error with: {error}");
                continue;
            }
            Ok(None) => {
                info!("{client}: closed");
                None
            }
            Err(_) => {
                warn!("{client}: idle timeout");
                None
            }
        };
    }
}

async fn relay(
    client: &Client,
    rx: &mut UnboundedReceiver<Broadcast>,
    queue: &mut VecDeque<Message>,
    ws_sink: &mut SplitSink<WebSocketStream<TcpStream>, Message>,
) {
    while let Some(mut broadcast) = rx.recv().await {
        let mut seen_bytes = 0;
        let mut dropped_bytes = 0;
        loop {
            let Broadcast::Message(message) = broadcast else {
                info!("{client}: received interrupt");
                queue.push_back(Message::Close(None));
                break;
            };
            seen_bytes += message.len();
            if seen_bytes > client.arguments.buffer_size as usize
                && let Some(dropped) = queue.pop_front()
            {
                let bytes = dropped.len();
                dropped_bytes += bytes;
                seen_bytes -= bytes;
            }
            queue.push_back(message);
            let Ok(next) = rx.try_recv() else {
                break;
            };
            broadcast = next;
        }
        if dropped_bytes > 0 {
            warn!("{client}: dropping {dropped_bytes} bytes to free the queue");
        }
        send(client, queue, ws_sink).await;
    }
}

async fn send(
    client: &Client,
    queue: &mut VecDeque<Message>,
    ws_sink: &mut SplitSink<WebSocketStream<TcpStream>, Message>,
) {
    let mut drain = iter(queue.drain(..).map(Ok));
    let send_result = timeout(
        Duration::from_secs(client.arguments.send_timeout),
        ws_sink.send_all(&mut drain),
    )
    .await;
    match send_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            error!("{client}: send error: {error}");
        }
        Err(_) => {
            warn!("{client}: send timeout");
        }
    }
}

#[derive(Debug, Default, Clone)]
struct State {
    rooms: Arc<RwLock<HashMap<RoomId, Room>>>,
}

impl State {
    async fn interrupt(&self) {
        let rooms = self.rooms.read().await;
        for room in rooms.values() {
            for client_target in room.clients.values() {
                let _ = client_target.tx.send(Broadcast::Interruption);
            }
        }
    }
}

#[derive(Debug)]
struct Client {
    arguments: Arguments,
    state: State,
    client_id: ClientId,
    room_id: RoomId,
}

impl Display for Client {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{client_id} ({room_id})",
            client_id = &self.client_id,
            room_id = &self.room_id,
        )
    }
}

impl Client {
    async fn register(&self, tx: UnboundedSender<Broadcast>) {
        let mut rooms = self.state.rooms.write().await;
        let room = rooms
            .entry(self.room_id.clone())
            .or_insert_with_key(|room_id| {
                info!("new room {room_id}");
                Room::default()
            });
        info!(
            "new client {client_id} in room {room_id}",
            client_id = &self.client_id,
            room_id = &self.room_id,
        );
        if room
            .clients
            .insert(self.client_id.clone(), ClientTarget { tx })
            .is_some()
        {
            warn!("{self}: collision");
        }
    }

    async fn unregister(&self) {
        let mut rooms = self.state.rooms.write().await;
        match rooms.get_mut(&self.room_id) {
            Some(room) => {
                if room.clients.remove(&self.client_id).is_some() {
                    info!(
                        "removed client {client_id} in room {room_id}",
                        client_id = &self.client_id,
                        room_id = &self.room_id,
                    );
                } else {
                    warn!("{self}: can't unregister: invalid client id");
                }
                if room.clients.is_empty() {
                    rooms.remove(&self.room_id);
                    info!("removed room {room_id}", room_id = &self.room_id);
                }
            }
            None => {
                warn!("{self}: can't unregister: invalid room id");
            }
        }
    }

    async fn broadcast(&self, message: Message) {
        if !matches!(&message, Message::Close(_)) {
            let rooms = self.state.rooms.read().await;
            if let Some(room) = rooms.get(&self.room_id) {
                for (client_id, client_target) in &room.clients {
                    if client_id != &self.client_id {
                        let _ = client_target.tx.send(Broadcast::Message(message.clone()));
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientId {
    socket_addr: SocketAddr,
    uuid: Uuid,
}

impl Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{socket_addr}|{uuid}",
            socket_addr = self.socket_addr,
            uuid = self.uuid,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RoomId {
    uri: Option<Uri>,
}

impl Display for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.uri {
            Some(uri) => write!(f, "{uri}"),
            None => write!(f, "<unknown room>"),
        }
    }
}

#[derive(Debug, Default)]
struct Room {
    clients: HashMap<ClientId, ClientTarget>,
}

#[derive(Debug)]
struct ClientTarget {
    tx: UnboundedSender<Broadcast>,
}

#[derive(Debug, Clone)]
enum Broadcast {
    Message(Message),
    Interruption,
}
