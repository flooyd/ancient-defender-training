mod protocol;

use futures_util::{SinkExt, StreamExt};
use protocol::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_tungstenite::{accept_async, tungstenite::Message};

type Players = Arc<RwLock<HashMap<u32, Player>>>;

struct Server {
    players: Players,
    next_id: Arc<RwLock<u32>>,
    tx: broadcast::Sender<ServerMessage>,
}

impl Server {
    fn new() -> Self {
        let (tx, _rx) = broadcast::channel(100);
        Server {
            players: Arc::new(RwLock::new(HashMap::new())),
            next_id: Arc::new(RwLock::new(1)),
            tx,
        }
    }

    async fn handle_client<S>(&self, tcp: S)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let ws_stream = match accept_async(tcp).await {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!("WebSocket handshake failed: {}", e);
                return;
            }
        };

        let mut player_id: Option<u32> = None;
        let mut authenticated = false;
        let mut rx = self.tx.subscribe();
        let (mut sink, mut stream) = ws_stream.split();

        let (client_tx, mut client_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let client_tx_bcast = client_tx.clone();

        tokio::spawn(async move {
            while let Ok(msg) = rx.recv().await {
                if client_tx_bcast.send(msg).is_err() { break; }
            }
        });

        tokio::spawn(async move {
            while let Some(msg) = client_rx.recv().await {
                if let Ok(data) = bincode::serialize(&msg) {
                    if sink.send(Message::Binary(data)).await.is_err() { break; }
                }
            }
        });

        while let Some(frame) = stream.next().await {
            let data = match frame {
                Ok(Message::Binary(d)) => d,
                Ok(Message::Close(_)) | Err(_) => break,
                _ => continue,
            };

            let msg = match bincode::deserialize::<ClientMessage>(&data) {
                Ok(m) => m,
                Err(_) => continue,
            };

            match msg {
                ClientMessage::Join { name, password } => {
                    let expected_pw = std::env::var("SERVER_PASSWORD").unwrap_or_default();
                    if !expected_pw.is_empty() && password != expected_pw {
                        let _ = client_tx.send(ServerMessage::Rejected {
                            reason: "Wrong password.".to_string(),
                        });
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        break;
                    }
                    authenticated = true;
                    let id = {
                        let mut next_id = self.next_id.write().await;
                        let id = *next_id;
                        *next_id += 1;
                        id
                    };
                    player_id = Some(id);

                    let display_name = if name.trim().is_empty() {
                        format!("Player{}", id)
                    } else {
                        name.trim().chars().take(20).collect()
                    };

                    let player = Player {
                        id,
                        name: display_name,
                        x: 0.0,
                        y: 0.0,
                        color: (
                            (id * 50 % 255) as u8,
                            (id * 100 % 255) as u8,
                            (id * 150 % 255) as u8,
                        ),
                    };

                    {
                        let mut players = self.players.write().await;
                        players.insert(id, player.clone());
                    }

                    let _ = client_tx.send(ServerMessage::Welcome { player_id: id });
                    let players_vec: Vec<Player> = {
                        let players = self.players.read().await;
                        players.values().cloned().collect()
                    };
                    let _ = client_tx.send(ServerMessage::WorldState { players: players_vec });
                    let _ = self.tx.send(ServerMessage::PlayerJoined { player });
                    println!("Player {} joined", id);
                }
                ClientMessage::Move { x, y } => {
                    if !authenticated { continue; }
                    if let Some(id) = player_id {
                        {
                            let mut players = self.players.write().await;
                            if let Some(player) = players.get_mut(&id) {
                                player.x = x;
                                player.y = y;
                            }
                        }
                        let _ = self.tx.send(ServerMessage::PlayerMoved { player_id: id, x, y });
                    }
                }
                ClientMessage::Disconnect => break,
            }
        }

        if let Some(id) = player_id {
            {
                let mut players = self.players.write().await;
                players.remove(&id);
            }
            let _ = self.tx.send(ServerMessage::PlayerLeft { player_id: id });
            println!("Player {} disconnected", id);
        }
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let bind_addr = format!("0.0.0.0:{}", port);

    let cert_path = std::env::var("TLS_CERT_PATH").ok();
    let key_path = std::env::var("TLS_KEY_PATH").ok();
    let tls_acceptor: Option<tokio_rustls::TlsAcceptor> = match (cert_path, key_path) {
        (Some(c), Some(k)) => {
            println!("TLS enabled (wss://)");
            Some(build_tls_acceptor(&c, &k))
        }
        _ => {
            println!("TLS disabled, running plain ws://");
            None
        }
    };

    let server = Arc::new(Server::new());
    let listener = TcpListener::bind(&bind_addr).await.expect("Failed to bind");
    let protocol = if tls_acceptor.is_some() { "wss" } else { "ws" };
    println!("WebSocket server listening on {}://0.0.0.0:{}", protocol, port);

    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                println!("New connection from: {}", addr);
                let server_clone = Arc::clone(&server);
                let acceptor = tls_acceptor.clone();
                tokio::spawn(async move {
                    match acceptor {
                        Some(tls) => match tls.accept(socket).await {
                            Ok(tls_stream) => server_clone.handle_client(tls_stream).await,
                            Err(e) => eprintln!("TLS handshake failed: {}", e),
                        },
                        None => server_clone.handle_client(socket).await,
                    }
                });
            }
            Err(e) => eprintln!("Accept error: {}", e),
        }
    }
}

fn build_tls_acceptor(cert_path: &str, key_path: &str) -> tokio_rustls::TlsAcceptor {
    use rustls_pemfile::{certs, private_key};
    use std::fs::File;
    use std::io::BufReader;
    use tokio_rustls::rustls::ServerConfig;

    let cert_file = File::open(cert_path).expect("Cannot open certificate file");
    let cert_chain: Vec<_> = certs(&mut BufReader::new(cert_file))
        .map(|r| r.expect("Failed to parse certificate"))
        .collect();

    let key_file = File::open(key_path).expect("Cannot open key file");
    let private_key = private_key(&mut BufReader::new(key_file))
        .expect("Failed to read key file")
        .expect("No private key found in file");

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .expect("Invalid TLS certificate/key");

    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}