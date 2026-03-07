mod protocol;

use futures_util::{SinkExt, StreamExt};
use protocol::*;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_tungstenite::{accept_async, tungstenite::Message};

type Players = Arc<RwLock<HashMap<u32, Player>>>;

/// Server-side movement state for a single creep.
struct CreepUnit {
    creep: Creep,
    /// Unit vector pointing from spawn toward center (0,0).
    lane_dx: f32,
    lane_dy: f32,
    /// Pre-assigned hold slot near (0,0); set at spawn, used on arrival.
    hold_x: f32,
    hold_y: f32,
    /// false = marching to center along lane; true = settled at hold slot.
    holding: bool,
    /// Id of the enemy creep this unit is currently attacking; None = no target.
    target_id: Option<u32>,
    /// Attack charge timer: 0.0 → 1.0.  Fires and resets when it reaches 1.0.
    attack_timer: f32,
}

type PatrolCreeps = Arc<RwLock<Vec<CreepUnit>>>;

fn spawn_patrol_creeps(start_id: u32, occupied: &[(f32, f32)]) -> Vec<CreepUnit> {
    let mut rng = rand::thread_rng();
    // 4 per team: 3 melee + 1 ranged
    let templates: &[(&str, &str, f32)] = &[
        ("Green", "melee",  586.0),
        ("Green", "melee",  586.0),
        ("Green", "melee",  586.0),
        ("Green", "ranged", 336.0),
        ("Red",   "melee",  586.0),
        ("Red",   "melee",  586.0),
        ("Red",   "melee",  586.0),
        ("Red",   "ranged", 336.0),
    ];
    // Lateral x-positions for each creep within a row (6 slots per row).
    const X_SLOTS: [f32; 6] = [-150.0, -90.0, -30.0, 30.0, 90.0, 150.0];
    // Rows step outward from center by this amount.
    const ROW_SPACING: f32 = 80.0;
    // A candidate slot is blocked if any occupied slot is within this radius.
    const SLOT_RADIUS: f32 = 50.0;
    // Find the innermost unoccupied slot for a given team.
    // Within each row all 6 columns are tried (preferred column first);
    // only advances to the next row when the entire row is blocked.
    // Green holds toward +y (their base); Red toward -y (their base).
    let find_slot = |team: &str, pref_col: usize, claimed: &[(f32, f32)]| -> (f32, f32) {
        let sign: f32 = if team == "Green" { 1.0 } else { -1.0 };
        for row in 0u32..200 {
            let y = sign * (35.0 + row as f32 * ROW_SPACING);
            // Try preferred column first; then all other columns in order.
            let mut cols = vec![pref_col];
            for c in 0..X_SLOTS.len() { if c != pref_col { cols.push(c); } }
            for col in cols {
                let x = X_SLOTS[col];
                let free = !occupied.iter().chain(claimed.iter()).any(|&(ox, oy)| {
                    let dx = ox - x;
                    let dy = oy - y;
                    dx * dx + dy * dy < SLOT_RADIUS * SLOT_RADIUS
                });
                if free {
                    return (x, y);
                }
            }
        }
        // Fallback: should never be reached in practice.
        (X_SLOTS[pref_col], sign * 35.0)
    };

    // Claim slots one by one so creeps within the same new wave don't collide
    // with each other either.
    let mut claimed: Vec<(f32, f32)> = Vec::new();
    templates.iter().enumerate().map(|(i, (team, type_, health))| {
        let pref_col = i % 4; // preferred column index (0-3) within team
        let (hold_x, hold_y) = find_slot(team, pref_col, &claimed);
        claimed.push((hold_x, hold_y));
        // Spawn in a column along the march axis for a natural diagonal line.
        let (base_x, base_y): (f32, f32) = if *team == "Green" {
            (-5500.0, 5500.0)
        } else {
            (5500.0, -5500.0)
        };
        // Lane: unit vector from base toward (0,0).
        let base_len = (base_x * base_x + base_y * base_y).sqrt();
        let (fwd_x, fwd_y) = (-base_x / base_len, -base_y / base_len);
        // Perpendicular (rotate fwd 90° CCW) = lateral axis.
        let (lat_x, lat_y) = (-fwd_y, fwd_x);
        let depth_offsets = [0.0_f32, 80.0, 160.0, 240.0];
        let depth_offset = depth_offsets[pref_col];
        let lat_jitter = rng.gen_range(-20.0_f32..20.0);
        let sx = base_x + fwd_x * depth_offset + lat_x * lat_jitter;
        let sy = base_y + fwd_y * depth_offset + lat_y * lat_jitter;
        // Lane direction: unit vector from spawn toward (0,0).
        let len = (sx * sx + sy * sy).sqrt().max(1.0);
        let (lane_dx, lane_dy) = (-sx / len, -sy / len);
        // hold_x / hold_y already computed above via find_slot.
        CreepUnit {
            creep: Creep {
                id: start_id + i as u32,
                x: sx,
                y: sy,
                health: *health,
                type_: type_.to_string(),
                team: team.to_string(),
                attack_charge: 0.0,
            },
            lane_dx,
            lane_dy,
            hold_x,
            hold_y,
            holding: false,
            target_id: None,
            attack_timer: 0.0,
        }
    }).collect()
}

type Projectiles = Arc<RwLock<Vec<Projectile>>>;

struct Server {
    players: Players,
    patrol_creeps: PatrolCreeps,
    projectiles: Projectiles,
    next_proj_id: Arc<AtomicU32>,
    next_creep_id: Arc<AtomicU32>,
    next_id: Arc<RwLock<u32>>,
    tx: broadcast::Sender<ServerMessage>,
}

impl Server {
    fn new() -> Self {
        let (tx, _rx) = broadcast::channel(100);
        // First wave uses IDs 1-8; subsequent waves start at 9.
        const WAVE_SIZE: u32 = 8;
        Server {
            players: Arc::new(RwLock::new(HashMap::new())),
            patrol_creeps: Arc::new(RwLock::new(spawn_patrol_creeps(1, &[]))),
            projectiles: Arc::new(RwLock::new(Vec::new())),
            next_proj_id: Arc::new(AtomicU32::new(1)),
            next_creep_id: Arc::new(AtomicU32::new(1 + WAVE_SIZE)),
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
                ClientMessage::Join { name, password, team } => {
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

                    let color = if team == "Green" {
                        (34u8, 197u8, 94u8)
                    } else {
                        (220u8, 38u8, 38u8)
                    };

                    // Spawn player in front of their creep line (between own base
                    // and center). Green base is (-5500, 5500), Red is (5500, -5500).
                    // ~60% of the way from base to center keeps them ahead of creeps.
                    let (spawn_x, spawn_y) = if team == "Green" {
                        (-2200.0_f32, 2200.0_f32)
                    } else {
                        (2200.0_f32, -2200.0_f32)
                    };

                    let player = Player {
                        id,
                        name: display_name,
                        x: spawn_x,
                        y: spawn_y,
                        color,
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
                    let creeps_snapshot: Vec<Creep> = {
                        let patrol = self.patrol_creeps.read().await;
                        patrol.iter().map(|p| p.creep.clone()).collect()
                    };
                    let _ = client_tx.send(ServerMessage::WorldState { players: players_vec, creeps: creeps_snapshot });
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
                ClientMessage::Attack { creep_id } => {
                    if !authenticated { continue; }
                    if let Some(pid) = player_id {
                        let player_info = {
                            let players = self.players.read().await;
                            players.get(&pid).map(|p| (p.x, p.y, p.color))
                        };
                        let creep_team = {
                            let creeps = self.patrol_creeps.read().await;
                            creeps.iter().find(|p| p.creep.id == creep_id)
                                  .map(|p| p.creep.team.clone())
                        };
                        if let (Some((px, py, color)), Some(creep_team)) = (player_info, creep_team) {
                            // Block attacking own team's creeps
                            let player_team = if color == (34, 197, 94) { "Green" } else { "Red" };
                            if player_team != creep_team.as_str() {
                                let proj_id = self.next_proj_id.fetch_add(1, Ordering::Relaxed);
                                let proj = Projectile { id: proj_id, x: px, y: py, target_creep_id: creep_id, color, min_damage: 39.0, max_damage: 45.0 };
                                self.projectiles.write().await.push(proj);
                            }
                        }
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

    // ── Wave spawner: append a new set of creeps every 30 seconds ───────────────
    {
        let patrol_creeps = Arc::clone(&server.patrol_creeps);
        let next_creep_id = Arc::clone(&server.next_creep_id);
        let tx_wave = server.tx.clone();
        tokio::spawn(async move {
            const WAVE_SIZE: u32 = 8;
            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(15)
            );
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                // Collect all currently occupied hold slots before spawning.
                let occupied: Vec<(f32, f32)> = patrol_creeps.read().await
                    .iter().map(|u| (u.hold_x, u.hold_y)).collect();
                let start_id = next_creep_id.fetch_add(WAVE_SIZE, Ordering::Relaxed);
                let wave = spawn_patrol_creeps(start_id, &occupied);
                {
                    let mut creeps = patrol_creeps.write().await;
                    creeps.extend(wave);
                }
                // Broadcast the new set so all clients learn of the new IDs.
                let snapshot = patrol_creeps.read().await
                    .iter().map(|p| p.creep.clone()).collect::<Vec<_>>();
                let _ = tx_wave.send(ServerMessage::CreepsState { creeps: snapshot });
                println!("Wave spawned, starting id {}", start_id);
            }
        });
    }

    // ── Creep advance tick: every 2 s, holding creeps step one row forward ──────
    // Processed closest-to-center first so vacated slots cascade immediately.
    // Within the next row all 6 columns are tried (preferred = same column first)
    // so the front row is always fully occupied before back rows are used.
    {
        let patrol_creeps = Arc::clone(&server.patrol_creeps);
        tokio::spawn(async move {
            const ROW_SPACING: f32 = 80.0;
            const SLOT_RADIUS: f32 = 50.0;
            const INNER_Y:     f32 = 35.0; // abs-y of the innermost valid row
            const X_SLOTS: [f32; 6] = [-150.0, -90.0, -30.0, 30.0, 90.0, 150.0];
            let mut interval = tokio::time::interval(
                std::time::Duration::from_millis(500)
            );
            interval.tick().await; // burn the immediate tick
            loop {
                interval.tick().await;
                let mut creeps = patrol_creeps.write().await;
                // Live map of id → effective hold position (updated as we go).
                // Only holding creeps are included; marching creeps must NOT
                // block back-row holders from advancing to the front row.
                let mut hold_map: HashMap<u32, (f32, f32)> = creeps.iter()
                    .filter(|u| u.holding)
                    .map(|u| (u.creep.id, (u.hold_x, u.hold_y)))
                    .collect();
                // Sort holding indices: smallest abs(y) first.
                let mut indices: Vec<usize> = (0..creeps.len())
                    .filter(|&i| creeps[i].holding)
                    .collect();
                indices.sort_by(|&a, &b| {
                    creeps[a].hold_y.abs()
                        .partial_cmp(&creeps[b].hold_y.abs())
                        .unwrap()
                });
                for idx in indices {
                    let (is_green, hold_x, hold_y, my_id) = {
                        let u = &creeps[idx];
                        (u.creep.team == "Green", u.hold_x, u.hold_y, u.creep.id)
                    };
                    let next_y = if is_green {
                        hold_y - ROW_SPACING   // Green: toward smaller y (center)
                    } else {
                        hold_y + ROW_SPACING   // Red:   toward larger  y (center)
                    };
                    // Never advance past the innermost row.
                    if  is_green && next_y < INNER_Y  - 1.0 { continue; }
                    if !is_green && next_y > -INNER_Y + 1.0 { continue; }
                    // Try the same column first, then all other columns in order of
                    // proximity to hold_x — the front row must be fully filled
                    // before any creep remains in a back row.
                    let pref_col = X_SLOTS.iter().enumerate()
                        .min_by(|(_, &a), (_, &b)| {
                            (a - hold_x).abs().partial_cmp(&(b - hold_x).abs()).unwrap()
                        })
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    let mut col_order: Vec<usize> = vec![pref_col];
                    for c in 0..X_SLOTS.len() { if c != pref_col { col_order.push(c); } }
                    // Sort non-preferred columns by distance to current x so the
                    // creep moves as little laterally as possible.
                    col_order[1..].sort_by(|&a, &b| {
                        (X_SLOTS[a] - hold_x).abs()
                            .partial_cmp(&(X_SLOTS[b] - hold_x).abs())
                            .unwrap()
                    });
                    let mut target: Option<(f32, f32)> = None;
                    for col in col_order {
                        let nx = X_SLOTS[col];
                        let free = hold_map.iter().all(|(&oid, &(ox, oy))| {
                            if oid == my_id { return true; }
                            let dx = ox - nx;
                            let dy = oy - next_y;
                            dx * dx + dy * dy >= SLOT_RADIUS * SLOT_RADIUS
                        });
                        if free {
                            target = Some((nx, next_y));
                            break;
                        }
                    }
                    if let Some((nx, ny)) = target {
                        hold_map.insert(my_id, (nx, ny));
                        creeps[idx].hold_x = nx;
                        creeps[idx].hold_y = ny;
                    }
                }
            }
        });
    }

    // ── Creep patrol + projectile tick (~30 fps) ────────────────────────────────
    {
        let patrol_creeps = Arc::clone(&server.patrol_creeps);
        let projectiles   = Arc::clone(&server.projectiles);
        let players       = Arc::clone(&server.players);
        let next_proj_id  = Arc::clone(&server.next_proj_id);
        let tx_tick = server.tx.clone();
        tokio::spawn(async move {
            const CREEP_SPEED: f32 = 325.0;
            const PROJ_SPEED:  f32 = 900.0;
            const PROJ_HIT:    f32 = 20.0;
            let mut interval = tokio::time::interval(
                std::time::Duration::from_millis(33)
            );
            loop {
                interval.tick().await;
                let dt = 33.0 / 1000.0_f32;

                // ─ Move creeps ────────────────────────────────────────────────────────
                {
                    // Separation radius sized to creep visuals (melee 40w, ranged 56w).
                    const SEP_RADIUS:   f32 = 60.0;
                    // Lateral drift speed cap (world-units/s) from separation.
                    const SEP_LATERAL:  f32 = 280.0;
                    // Distance from (0,0) at which marching creeps switch to holding.
                    const ARRIVE_DIST:  f32 = 250.0;

                    let player_positions: Vec<(f32, f32)> = {
                        players.read().await.values().map(|p| (p.x, p.y)).collect()
                    };
                    let mut creeps = patrol_creeps.write().await;
                    // Pre-tick snapshot for symmetric, lag-free separation forces.
                    let snap: Vec<(u32, f32, f32)> = creeps.iter()
                        .map(|u| (u.creep.id, u.creep.x, u.creep.y))
                        .collect();
                    // Snapshot of holding slots so arriving creeps can detect
                    // whether their pre-assigned slot was stolen by the advance tick.
                    let holding_slots: Vec<(u32, f32, f32)> = creeps.iter()
                        .filter(|u| u.holding)
                        .map(|u| (u.creep.id, u.hold_x, u.hold_y))
                        .collect();

                    for unit in creeps.iter_mut() {
                        if unit.holding {
                            // Settle toward exact hold slot; freeze once within 2 units.
                            let dx = unit.hold_x - unit.creep.x;
                            let dy = unit.hold_y - unit.creep.y;
                            let dist = (dx * dx + dy * dy).sqrt();
                            if dist > 2.0 {
                                let step = (CREEP_SPEED * 0.5 * dt).min(dist);
                                unit.creep.x += (dx / dist) * step;
                                unit.creep.y += (dy / dist) * step;
                            } else {
                                // Snap to slot and stop permanently.
                                unit.creep.x = unit.hold_x;
                                unit.creep.y = unit.hold_y;
                            }
                            continue;
                        }

                        // ―― Marching phase ――
                        // 1. Constant forward velocity along the pre-computed lane direction.
                        let fwd_x = unit.lane_dx * CREEP_SPEED;
                        let fwd_y = unit.lane_dy * CREEP_SPEED;

                        // 2. Lateral-only separation (strip forward projection so the
                        //    march axis is never slowed or deflected by neighbours).
                        let mut sep_x = 0.0_f32;
                        let mut sep_y = 0.0_f32;
                        for &(other_id, ox, oy) in &snap {
                            if other_id == unit.creep.id { continue; }
                            let dx = unit.creep.x - ox;
                            let dy = unit.creep.y - oy;
                            let dist = (dx * dx + dy * dy).sqrt();
                            if dist < SEP_RADIUS && dist > 0.01 {
                                let w = (SEP_RADIUS - dist) / SEP_RADIUS;
                                sep_x += (dx / dist) * w;
                                sep_y += (dy / dist) * w;
                            }
                        }
                        for &(px, py) in &player_positions {
                            let dx = unit.creep.x - px;
                            let dy = unit.creep.y - py;
                            let dist = (dx * dx + dy * dy).sqrt();
                            if dist < SEP_RADIUS && dist > 0.01 {
                                let w = (SEP_RADIUS - dist) / SEP_RADIUS;
                                sep_x += (dx / dist) * w * 1.5;
                                sep_y += (dy / dist) * w * 1.5;
                            }
                        }
                        // Remove the forward (lane) component from separation.
                        let fwd_dot = sep_x * unit.lane_dx + sep_y * unit.lane_dy;
                        sep_x -= fwd_dot * unit.lane_dx;
                        sep_y -= fwd_dot * unit.lane_dy;

                        // 3. Apply: constant forward + capped lateral drift.
                        unit.creep.x += (fwd_x + sep_x * SEP_LATERAL) * dt;
                        unit.creep.y += (fwd_y + sep_y * SEP_LATERAL) * dt;

                        // 4. Arrival: switch to holding when close to (0,0).
                        let dist_center = (unit.creep.x * unit.creep.x
                            + unit.creep.y * unit.creep.y).sqrt();
                        if dist_center < ARRIVE_DIST {
                            // The advance tick may have placed a holding creep in
                            // this marching creep's pre-assigned slot.  Re-find the
                            // innermost free slot so we never double-occupy.
                            const X_SLOTS_ARR: [f32; 6] =
                                [-150.0, -90.0, -30.0, 30.0, 90.0, 150.0];
                            const ROW_SPACING_ARR: f32 = 80.0;
                            const SLOT_RADIUS_ARR: f32 = 50.0;
                            let is_green = unit.creep.team == "Green";
                            let sign: f32 = if is_green { 1.0 } else { -1.0 };
                            // Check if pre-assigned slot is already occupied.
                            let slot_taken = holding_slots.iter().any(|&(oid, ox, oy)| {
                                if oid == unit.creep.id { return false; }
                                let dx = ox - unit.hold_x;
                                let dy = oy - unit.hold_y;
                                dx * dx + dy * dy < SLOT_RADIUS_ARR * SLOT_RADIUS_ARR
                            });
                            if slot_taken {
                                // Find innermost free slot for this team.
                                'find: for row in 0u32..200 {
                                    let y = sign * (35.0 + row as f32 * ROW_SPACING_ARR);
                                    for &x in &X_SLOTS_ARR {
                                        let free = holding_slots.iter().all(|&(oid, ox, oy)| {
                                            if oid == unit.creep.id { return true; }
                                            let dx = ox - x;
                                            let dy = oy - y;
                                            dx * dx + dy * dy >= SLOT_RADIUS_ARR * SLOT_RADIUS_ARR
                                        });
                                        if free {
                                            unit.hold_x = x;
                                            unit.hold_y = y;
                                            break 'find;
                                        }
                                    }
                                }
                            }
                            unit.holding = true;
                        }
                    }
                }

                // ─ Creep-vs-creep combat ──────────────────────────────────────────
                // NOTE: ThreadRng is !Send, so it must never be alive across an
                // `.await` point.  Each inner block drops it before the next await.
                {
                    // Step 1: take a read snapshot (async – no rng yet)
                    // Tuple: (id, team, x, y, lane_dx, lane_dy, holding, hold_y)
                    let snap: Vec<(u32, String, f32, f32, f32, f32, bool, f32)> = {
                        let creeps = patrol_creeps.read().await;
                        creeps.iter().map(|u| {
                            (u.creep.id, u.creep.team.clone(),
                             u.creep.x, u.creep.y,
                             u.lane_dx, u.lane_dy,
                             u.holding, u.hold_y)
                        }).collect()
                    };

                    // Step 2: mutate creep units + accumulate events (one async
                    // write lock – rng created AFTER the await, dropped before
                    // the next one).
                    let mut melee_damage_queue: Vec<(u32, f32)> = Vec::new();
                    let mut new_projectiles:    Vec<Projectile> = Vec::new();
                    {
                        let mut creeps = patrol_creeps.write().await;
                        // rng lives entirely within this sync block (no more awaits).
                        let mut rng = rand::thread_rng();
                        for unit in creeps.iter_mut() {
                            // Only creeps settled on the front row (innermost hold slot,
                            // abs-y ≈ 35) may attack.  INNER_Y=35, ROW_SPACING=80, so
                            // the midpoint to the next row is 75.  Marching creeps and
                            // back-row creeps cannot attack; reset their state.
                            let on_front_row = unit.holding && unit.hold_y.abs() < 75.0;
                            if !on_front_row {
                                unit.target_id = None;
                                unit.attack_timer = 0.0;
                                unit.creep.attack_charge = 0.0;
                                continue;
                            }

                            // Validate current target
                            let target_alive = unit.target_id.map(|tid| {
                                snap.iter().any(|(id, team, ..)| {
                                    *id == tid && *team != unit.creep.team
                                })
                            }).unwrap_or(false);
                            if !target_alive {
                                unit.target_id = None;
                                unit.attack_timer = 0.0;
                            }

                            // Acquire a new random target when we have none
                            if unit.target_id.is_none() {
                                let candidates: Vec<u32> = snap.iter().filter_map(|(id, team, tx, ty, _, _, t_holding, t_hold_y)| {
                                    if *team == unit.creep.team { return None; }
                                    // Target must also be on its own front row.
                                    if !t_holding || t_hold_y.abs() >= 75.0 { return None; }
                                    let dx = tx - unit.creep.x;
                                    let dy = ty - unit.creep.y;
                                    let fwd = dx * unit.lane_dx + dy * unit.lane_dy;
                                    if fwd <= 0.0 { return None; }
                                    let lat = (dx * unit.lane_dy - dy * unit.lane_dx).abs();
                                    if unit.creep.type_ == "melee" {
                                        // One row ahead (ROW_SPACING=80) + lateral ≤ one slot gap.
                                        if fwd <= 120.0 && lat <= 130.0 { Some(*id) } else { None }
                                    } else {
                                        // Ranged: same one-row forward limit, wider lateral.
                                        if lat <= 200.0 && fwd <= 120.0 { Some(*id) } else { None }
                                    }
                                }).collect();
                                if !candidates.is_empty() {
                                    let pick = rng.gen_range(0..candidates.len());
                                    unit.target_id = Some(candidates[pick]);
                                }
                            }

                            // Advance timer and fire
                            if let Some(target_id) = unit.target_id {
                                unit.attack_timer += dt;
                                if unit.attack_timer >= 1.0 {
                                    unit.attack_timer -= 1.0;
                                    if unit.creep.type_ == "melee" {
                                        let dmg = rng.gen_range(19.0_f32..=23.0);
                                        melee_damage_queue.push((target_id, dmg));
                                    } else {
                                        let target_pos = snap.iter()
                                            .find(|(id, ..)| *id == target_id)
                                            .map(|(_, _, tx, ty, ..)| (*tx, *ty));
                                        if target_pos.is_some() {
                                            let proj_id = next_proj_id
                                                .fetch_add(1, Ordering::Relaxed);
                                            let color = if unit.creep.team == "Green" {
                                                (34u8, 197u8, 94u8)
                                            } else {
                                                (220u8, 38u8, 38u8)
                                            };
                                            new_projectiles.push(Projectile {
                                                id:              proj_id,
                                                x:               unit.creep.x,
                                                y:               unit.creep.y,
                                                target_creep_id: target_id,
                                                color,
                                                min_damage: 21.0,
                                                max_damage: 26.0,
                                            });
                                        }
                                    }
                                }
                                unit.creep.attack_charge = unit.attack_timer;
                            } else {
                                unit.attack_timer = 0.0;
                                unit.creep.attack_charge = 0.0;
                            }
                        }
                        // Apply melee damage
                        for (tid, dmg) in &melee_damage_queue {
                            if let Some(u) = creeps.iter_mut().find(|u| u.creep.id == *tid) {
                                u.creep.health = (u.creep.health - dmg).max(0.0);
                            }
                        }
                        // rng + creeps write-guard both drop here – before next await
                    }

                    // Step 3: append new ranged projectiles (separate async write)
                    if !new_projectiles.is_empty() {
                        projectiles.write().await.extend(new_projectiles);
                    }
                }

                // ─ Move projectiles, collect hits ────────────────────────────────
                let mut damage_queue: Vec<(u32, f32)> = Vec::new();
                let proj_snapshot = {
                    let mut projs = projectiles.write().await;
                    let creeps    = patrol_creeps.read().await;
                    let mut kept  = Vec::new();
                    for mut proj in projs.drain(..) {
                        if let Some(unit) = creeps.iter().find(|p| p.creep.id == proj.target_creep_id) {
                            let dx   = unit.creep.x - proj.x;
                            let dy   = unit.creep.y - proj.y;
                            let dist = (dx * dx + dy * dy).sqrt();
                            if dist < PROJ_HIT {
                                let dmg = rand::thread_rng()
                                    .gen_range(proj.min_damage..=proj.max_damage);
                                damage_queue.push((proj.target_creep_id, dmg));
                            } else {
                                let step = (PROJ_SPEED * dt).min(dist);
                                proj.x += (dx / dist) * step;
                                proj.y += (dy / dist) * step;
                                kept.push(proj);
                            }
                        }
                        // target not found (dead creep) → discard projectile
                    }
                    *projs = kept.clone();
                    kept
                };

                // ─ Apply damage, remove dead creeps, build snapshot ──────────────
                let creep_snapshot = {
                    let mut creeps = patrol_creeps.write().await;
                    for (creep_id, dmg) in damage_queue {
                        if let Some(p) = creeps.iter_mut().find(|p| p.creep.id == creep_id) {
                            p.creep.health = (p.creep.health - dmg).max(0.0);
                        }
                    }
                    creeps.retain(|p| p.creep.health > 0.0);
                    creeps.iter().map(|p| p.creep.clone()).collect::<Vec<_>>()
                };

                let _ = tx_tick.send(ServerMessage::CreepsState { creeps: creep_snapshot });
                let _ = tx_tick.send(ServerMessage::ProjectilesState { projectiles: proj_snapshot });
            }
        });
    }

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