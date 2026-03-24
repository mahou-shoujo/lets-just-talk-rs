use axum::{
    Router,
    body::Body,
    extract::{
        Path, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderName, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
};
use axum_server::tls_rustls::RustlsConfig;
use futures_util::{SinkExt, StreamExt, future::BoxFuture, stream};
use log::{info, warn};
use std::{
    collections::HashMap, env, error::Error, net::SocketAddr, path::PathBuf, sync::Arc,
    time::Duration,
};
use tokio::{
    fs::read,
    io::Error as IoError,
    runtime::Runtime,
    select,
    sync::{
        RwLock,
        mpsc::{Sender, channel},
        oneshot,
    },
    time::sleep,
};
use uuid::Uuid;

const WAIT_BOUNCE: Duration = Duration::from_secs(1);

const WAIT_AUTOREMOVE: Duration = Duration::from_secs(15);

struct State {
    rooms: HashMap<String, Vec<Client>>,
}

struct Client {
    id: Uuid,
    tx: Option<Sender<Message>>,
}

pub fn main() -> Result<(), Box<dyn Error>> {
    simple_logger::init_with_env()?;
    let rt = Arc::new(Runtime::new()?);
    let state = Arc::new(RwLock::new(State {
        rooms: HashMap::new(),
    }));
    let (tx, rx) = oneshot::channel();
    let _ = ctrlc::set_handler({
        let rt = rt.clone();
        let state = state.clone();
        let mut tx = Some(tx);
        move || {
            if let Some(tx) = tx.take() {
                let state = state.clone();
                rt.spawn(async move {
                    {
                        let mut state = state.write().await;
                        for clients in state.rooms.values_mut() {
                            for client in clients {
                                if let Some(tx) = client.tx.take() {
                                    let _ = tx.try_send(Message::Close(None));
                                }
                                info!("Sending close to client {}", client.id);
                            }
                        }
                    }
                    sleep(WAIT_BOUNCE).await;
                    let _ = tx.send(());
                });
            }
        }
    });
    rt.block_on(async move {
        let future = serve(state).await?;
        tokio::select! {
            _ = rx => {}
            result = future => {
                result?;
            },
        }
        Ok(())
    })
}

async fn serve(
    state: Arc<RwLock<State>>,
) -> Result<BoxFuture<'static, Result<(), IoError>>, Box<dyn Error>> {
    let cert = env::var("CERT");
    let cert_key = env::var("CERT_KEY");
    let use_tls = cert.is_ok() || cert_key.is_ok();
    let app = Router::new()
        .route("/resource/{resource}", get(resource_handler))
        .route("/chat/{room}", get(root_handler))
        .route(
            "/chat/{room}/websocket",
            any(move |Path(room): Path<String>, ws| websocket_handler(room, ws, state)),
        );
    let port = if use_tls { 443 } else { 3000 };
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    Ok(if use_tls {
        let config =
            RustlsConfig::from_pem_file(PathBuf::from(cert?), PathBuf::from(cert_key?)).await?;
        Box::pin(axum_server::bind_rustls(addr, config).serve(app.into_make_service()))
    } else {
        Box::pin(axum_server::bind(addr).serve(app.into_make_service()))
    })
}

async fn resource_handler(Path(resource): Path<String>) -> Response {
    file(resource)
}

async fn root_handler(Path(_): Path<String>) -> Response {
    file("index.html".to_owned())
}

async fn websocket_handler(
    room: String,
    ws: WebSocketUpgrade,
    state: Arc<RwLock<State>>,
) -> Response {
    ws.on_upgrade(move |socket| websocket_handle(room, socket, state))
}

async fn websocket_handle(room: String, websocket: WebSocket, state: Arc<RwLock<State>>) {
    let id = Uuid::new_v4();
    let (mut ws_tx, mut ws_rx) = websocket.split();
    let (tx, mut rx) = channel(1024);
    {
        let mut state = state.write().await;
        state
            .rooms
            .entry(room.clone())
            .or_insert_with_key(|room| {
                info!("Added a new room: {room}");
                Vec::new()
            })
            .push(Client { id, tx: Some(tx) });
    }
    info!("Added a new client: {id}");
    tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if let Err(error) = ws_tx.send(message).await {
                warn!("Unable to broadcast to {id}: {error}");
            }
        }
        info!("Shut down broadcast to {id}");
    });
    let mut must_close = false;
    loop {
        select! {
            next = ws_rx.next() => {
                let Some(result) = next else {
                    break;
                };
                match result {
                    Ok(message) if matches!(&message, Message::Close(_)) => {}
                    Ok(message) => {
                        let state = state.read().await;
                        for client in state.rooms.get(&room).into_iter().flatten() {
                            if (client.id != id || room == "echo")
                                && let Some(tx) = &client.tx
                            {
                                let _ = tx.try_send(message.clone());
                            }
                        }
                    }
                    Err(error) => {
                        warn!("Unable to read from {id}: {error}");
                    }
                }
            }
            _ = sleep(WAIT_AUTOREMOVE) => {
                must_close = true;
                break;
            }
        }
    }

    let mut state = state.write().await;
    let tx = if let Some(mut clients) = state.rooms.remove(&room) {
        let mut found = false;
        let mut found_what = None;
        clients.retain_mut(|client| {
            let matches = client.id == id;
            found |= matches;
            if matches && let Some(tx) = client.tx.take() {
                found_what = Some(tx);
            }
            !matches
        });
        if found {
            info!("Dropped client {id}");
        } else {
            warn!("Dropped nonexistent client {id}");
        }
        if !clients.is_empty() {
            state.rooms.insert(room, clients);
        } else {
            info!("Dropped room {room}");
        }
        found_what
    } else {
        None
    };
    if must_close && let Some(tx) = tx {
        let _ = tx.try_send(Message::Close(None));
        sleep(WAIT_BOUNCE).await;
    }
}

fn file(resource: String) -> Response {
    let mime = match resource.as_str() {
        "index.html" => Some("text/html"),
        "audio.js" => Some("application/javascript"),
        _ => None,
    };
    match mime {
        Some(mime) => Response::builder()
            .header(HeaderName::from_static("content-type"), mime)
            .body(Body::from_stream(stream::once(async move {
                read(resource).await
            })))
            .expect("invalid response"),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
