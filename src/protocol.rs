use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Player {
    pub id: u32,
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub color: (u8, u8, u8),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Creep {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub health: f32,
    pub type_: String,
    pub team: String,
    /// 0.0–1.0 charge fraction toward the next attack (drives darkening animation).
    pub attack_charge: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Projectile {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub target_creep_id: u32,
    pub color: (u8, u8, u8), // team color of the attacker
    /// Damage range rolled on impact.
    pub min_damage: f32,
    pub max_damage: f32,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    Join { name: String, password: String, team: String },
    Move { x: f32, y: f32 },
    Attack { creep_id: u32 },
    Disconnect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]  // Added Clone here
pub enum ServerMessage {
    Welcome { player_id: u32 },
    Rejected { reason: String },
    PlayerJoined { player: Player },
    PlayerMoved { player_id: u32, x: f32, y: f32 },
    PlayerLeft { player_id: u32 },
    WorldState { players: Vec<Player>, creeps: Vec<Creep> },
    CreepsState { creeps: Vec<Creep> },
    ProjectilesState { projectiles: Vec<Projectile> },
}

#[allow(dead_code)]
pub const SERVER_ADDR: &str = "127.0.0.1:8080";  // TCP bind address for the server

/// WebSocket URL for the client.
/// - Local:  ws://127.0.0.1:8080  (default)
/// - Render: wss://your-server.onrender.com  (set via build env var)
#[allow(dead_code)]
pub const WS_URL: &str = if let Some(url) = option_env!("WS_URL") {
    url
} else {
    "ws://127.0.0.1:8080"
};