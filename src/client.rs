mod protocol;

use macroquad::prelude::*;
use protocol::*;
use std::collections::HashMap;

// Dota 2 turn rate is a per-tick lerp factor at 30 ticks/s:
//   new_facing = old_facing + turn_rate * remaining_angle   (each tick)
// This gives the characteristic exponential-decay "floaty" Dota turn feel.
const INVOKER_SPEED: f32 = 285.0;   // world-units per second
const INVOKER_TURN_RATE: f32 = 0.5; // fraction of remaining angle closed per tick
const DOTA_TICK_RATE: f32 = 30.0;   // Dota 2 server tick rate

/// Signed angle from `from` to `to`, in [-π, π].
fn angle_diff(from: f32, to: f32) -> f32 {
    let diff = (to - from).rem_euclid(std::f32::consts::TAU);
    if diff > std::f32::consts::PI { diff - std::f32::consts::TAU } else { diff }
}

fn creep_max_health(type_: &str) -> f32 {
    if type_ == "melee" { 586.0 } else { 336.0 }
}

/// Dota 2-style lerp turn: closes `INVOKER_TURN_RATE` fraction of the
/// remaining angle each game tick (30/s), giving exponential decay.
fn dota_turn_toward(current: f32, desired: f32, delta: f32) -> f32 {
    let diff = angle_diff(current, desired);
    if diff == 0.0 { return current; }
    // decay remaining angle by (1 - turn_rate) per tick over `delta` seconds
    let remaining_frac = (1.0_f32 - INVOKER_TURN_RATE).powf(delta * DOTA_TICK_RATE);
    current + diff * (1.0 - remaining_frac)
}

// ─── Platform-specific WebSocket wrapper ─────────────────────────────────────
//
// Native: tungstenite (sync) runs in a background thread; main thread
//         communicates via std::sync::mpsc channels.
// WASM:   quad-net WebSocket wraps the browser WebSocket API.

#[cfg(not(target_arch = "wasm32"))]
mod platform_ws {
    use std::sync::mpsc::{self, Receiver, Sender};
    use tungstenite::{connect, Message};

    pub struct WsClient {
        sender: Sender<Vec<u8>>,
        receiver: Receiver<Vec<u8>>,
    }

    impl WsClient {
        /// Connects synchronously (blocks until handshake completes or fails).
        pub fn connect(url: &str) -> Result<Self, String> {
            let (mut ws, _) = connect(url).map_err(|e| e.to_string())?;

            let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
            let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>();

            // Set a short read timeout on the underlying plain TCP stream so
            // the worker thread can drain the send queue even when idle.
            // (Only works for ws://, which is all we need here.)
            #[allow(irrefutable_let_patterns)]
            if let tungstenite::stream::MaybeTlsStream::Plain(tcp) = ws.get_mut() {
                tcp.set_read_timeout(Some(std::time::Duration::from_millis(5)))
                    .ok();
            }

            std::thread::spawn(move || loop {
                // Drain outgoing queue
                while let Ok(data) = out_rx.try_recv() {
                    if ws.send(Message::Binary(data)).is_err() {
                        return;
                    }
                }
                // Try to read one incoming frame (returns quickly on timeout)
                match ws.read() {
                    Ok(Message::Binary(data)) => {
                        in_tx.send(data).ok();
                    }
                    Ok(Message::Close(_)) => return,
                    Err(tungstenite::Error::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => return,
                    _ => {}
                }
            });

            Ok(WsClient {
                sender: out_tx,
                receiver: in_rx,
            })
        }

        pub fn send_bytes(&self, data: &[u8]) {
            self.sender.send(data.to_vec()).ok();
        }

        pub fn try_recv(&mut self) -> Option<Vec<u8>> {
            self.receiver.try_recv().ok()
        }

        /// On native, connecting is synchronous so we're already connected.
        pub fn connected(&self) -> bool {
            true
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod platform_ws {
    pub use quad_net::web_socket::WebSocket as WsClient;
}

use platform_ws::WsClient;

fn send_msg(ws: &WsClient, msg: &ClientMessage) {
    if let Ok(data) = bincode::serialize(msg) {
        ws.send_bytes(&data);
    }
}

// ─── Win32-only: focus, cursor confinement, physical mouse position ───────────

#[cfg(target_os = "windows")]
fn is_app_focused() -> bool {
    extern "system" {
        fn GetForegroundWindow() -> *mut std::ffi::c_void;
        fn GetWindowThreadProcessId(hwnd: *mut std::ffi::c_void, pid: *mut u32) -> u32;
        fn GetCurrentProcessId() -> u32;
    }
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        pid == GetCurrentProcessId()
    }
}

#[cfg(not(target_os = "windows"))]
fn is_app_focused() -> bool {
    true
}

#[cfg(target_os = "windows")]
fn cursor_and_screen() -> (i32, i32, i32, i32) {
    #[repr(C)]
    struct POINT {
        x: i32,
        y: i32,
    }
    extern "system" {
        fn GetCursorPos(pt: *mut POINT) -> i32;
        fn GetSystemMetrics(n: i32) -> i32;
    }
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        let sw = GetSystemMetrics(0);
        let sh = GetSystemMetrics(1);
        (pt.x, pt.y, sw, sh)
    }
}

#[cfg(not(target_os = "windows"))]
fn cursor_and_screen() -> (i32, i32, i32, i32) {
    let (mx, my) = mouse_position();
    (
        mx as i32,
        my as i32,
        screen_width() as i32,
        screen_height() as i32,
    )
}

#[cfg(target_os = "windows")]
fn set_cursor_confinement(confine: bool) {
    #[repr(C)]
    struct RECT {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    extern "system" {
        fn ClipCursor(rect: *const RECT) -> i32;
        fn GetSystemMetrics(n: i32) -> i32;
    }
    unsafe {
        if confine {
            let rect = RECT {
                left: 0,
                top: 0,
                right: GetSystemMetrics(0),
                bottom: GetSystemMetrics(1),
            };
            ClipCursor(&rect);
        } else {
            ClipCursor(std::ptr::null());
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn set_cursor_confinement(_confine: bool) {}

// ─── Game structs ─────────────────────────────────────────────────────────────

struct RemotePlayer {
    player: Player,
    target_x: f32,
    target_y: f32,
    visual_x: f32,
    visual_y: f32,
}

impl RemotePlayer {
    fn new(player: Player) -> Self {
        RemotePlayer {
            visual_x: player.x,
            visual_y: player.y,
            target_x: player.x,
            target_y: player.y,
            player,
        }
    }

    fn update_target(&mut self, x: f32, y: f32) {
        self.target_x = x;
        self.target_y = y;
    }

    fn interpolate(&mut self, delta: f32) {
        let lerp_speed = 10.0;
        self.visual_x += (self.target_x - self.visual_x) * lerp_speed * delta;
        self.visual_y += (self.target_y - self.visual_y) * lerp_speed * delta;
    }
}

struct CreepVisual {
    creep: Creep,
    target_x: f32,
    target_y: f32,
    visual_x: f32,
    visual_y: f32,
    /// Previous frame's attack_charge used to detect when an attack fires (charge wraps).
    prev_charge: f32,
    /// Counts down toward 0 after a melee attack fires; drives the sword-flash visual.
    sword_flash_timer: f32,
}

impl CreepVisual {
    fn new(creep: Creep) -> Self {
        CreepVisual {
            visual_x: creep.x,
            visual_y: creep.y,
            target_x: creep.x,
            target_y: creep.y,
            prev_charge: 0.0,
            sword_flash_timer: 0.0,
            creep,
        }
    }

    fn update_target(&mut self, x: f32, y: f32) {
        self.target_x = x;
        self.target_y = y;
    }

    fn interpolate(&mut self, delta: f32) {
        // Use a slightly higher lerp speed than players since creeps tick at 30fps
        let lerp_speed = 10.0;
        self.visual_x += (self.target_x - self.visual_x) * lerp_speed * delta;
        self.visual_y += (self.target_y - self.visual_y) * lerp_speed * delta;
    }
}

/// Smooth rendering for fast-moving projectiles between server ticks.
struct ProjectileVisual {
    proj: Projectile,
    visual_x: f32,
    visual_y: f32,
}

impl ProjectileVisual {
    fn new(proj: Projectile) -> Self {
        ProjectileVisual { visual_x: proj.x, visual_y: proj.y, proj }
    }

    /// Update authoritative position from the server tick.
    fn update_target(&mut self, x: f32, y: f32) {
        self.proj.x = x;
        self.proj.y = y;
    }

    /// Exponential lerp toward the server position; high speed because
    /// projectiles travel 900 u/s so gaps close very quickly.
    fn interpolate(&mut self, delta: f32) {
        let lerp_speed = 25.0;
        self.visual_x += (self.proj.x - self.visual_x) * lerp_speed * delta;
        self.visual_y += (self.proj.y - self.visual_y) * lerp_speed * delta;
    }
}

struct GameState {
    my_id: Option<u32>,
    my_name: String,
    my_color: (u8, u8, u8),
    remote_players: HashMap<u32, RemotePlayer>,
    my_pos: (f32, f32),
    /// Right-click destination in world space; None = standing still.
    move_target: Option<(f32, f32)>,
    /// Current facing angle in radians (0 = right, π/2 = down).
    facing_angle: f32,
    /// Creep the player is turning to face before firing; cleared once shot.
    attack_target: Option<u32>,
    creeps: HashMap<u32, CreepVisual>,
    projectiles: HashMap<u32, ProjectileVisual>,
}

impl GameState {
    fn new(name: String, color: (u8, u8, u8)) -> Self {
        GameState {
            my_id: None,
            my_name: name,
            my_color: color,
            remote_players: HashMap::new(),
            my_pos: (0.0, 0.0),
            move_target: None,
            facing_angle: 0.0,
            attack_target: None,
            creeps: HashMap::new(),
            projectiles: HashMap::new(),
        }
    }
}

// ─── Window config ────────────────────────────────────────────────────────────

fn window_conf() -> Conf {
    let mut conf = Conf {
        window_title: "Macroquad MMO Prototype".to_owned(),
        ..Default::default()
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        conf.fullscreen = true;
    }
    conf
}

// ─── Entry point ──────────────────────────────────────────────────────────────

#[macroquad::main(window_conf)]
async fn main() {
    let mut login_error: Option<String> = None;

    'login: loop {

    // ── Login screen (name + password + team) ────────────────────────────────
    let mut focus_name = true;
    let mut player_name = String::new();
    let mut password = String::new();
    // None = unselected, Some(true) = Green, Some(false) = Red
    let mut team_green: Option<bool> = None;
    loop {
        while let Some(c) = get_char_pressed() {
            if c.is_control() { continue; }
            if focus_name { if player_name.len() < 20 { player_name.push(c); } }
            else          { if password.len() < 30    { password.push(c); }    }
        }
        if is_key_pressed(KeyCode::Backspace) {
            if focus_name { player_name.pop(); } else { password.pop(); }
        }
        if is_key_pressed(KeyCode::Tab) {
            focus_name = !focus_name;
        }
        if is_key_pressed(KeyCode::Enter) {
            if focus_name && player_name.len() >= 3 {
                focus_name = false;
            } else if !focus_name && player_name.len() >= 3 && password.len() >= 3 && team_green.is_some() {
                break;
            }
        }

        let sw = screen_width();
        let sh = screen_height();
        clear_background(BLACK);

        if let Some(ref err) = login_error {
            let edim = measure_text(err, None, 22, 1.0);
            draw_text(err, sw / 2.0 - edim.width / 2.0, sh / 2.0 - 130.0, 22.0, RED);
        }
        let title = "Enter your details";
        let tdim = measure_text(title, None, 32, 1.0);
        draw_text(title, sw / 2.0 - tdim.width / 2.0, sh / 2.0 - 90.0, 32.0, PURPLE);

        // Name field
        {
            let color = if focus_name { WHITE } else { GRAY };
            draw_text("Name:", sw / 2.0 - 160.0, sh / 2.0 - 20.0, 20.0, color);
            let display = format!("{}{}", player_name, if focus_name { "_" } else { "" });
            draw_rectangle(sw / 2.0 - 60.0, sh / 2.0 - 42.0, 220.0, 30.0, Color::from_rgba(40, 40, 40, 255));
            draw_rectangle_lines(sw / 2.0 - 60.0, sh / 2.0 - 42.0, 220.0, 30.0, 2.0, color);
            draw_text(&display, sw / 2.0 - 50.0, sh / 2.0 - 20.0, 22.0, color);
        }

        // Password field
        {
            let color = if !focus_name { WHITE } else { GRAY };
            draw_text("Password:", sw / 2.0 - 160.0, sh / 2.0 + 30.0, 20.0, color);
            let pw_display = format!("{}{}", "\u{25cf}".repeat(password.len()), if !focus_name { "_" } else { "" });
            draw_rectangle(sw / 2.0 - 60.0, sh / 2.0 + 8.0, 220.0, 30.0, Color::from_rgba(40, 40, 40, 255));
            draw_rectangle_lines(sw / 2.0 - 60.0, sh / 2.0 + 8.0, 220.0, 30.0, 2.0, color);
            draw_text(&pw_display, sw / 2.0 - 50.0, sh / 2.0 + 30.0, 22.0, color);
        }

        // Team selection buttons
        {
            draw_text("Team:", sw / 2.0 - 160.0, sh / 2.0 + 80.0, 20.0, BLACK);
            let (mx, my) = mouse_position();
            let clicked = is_mouse_button_pressed(MouseButton::Left);

            // Green button
            let gb_x = sw / 2.0 - 60.0;
            let gb_y = sh / 2.0 + 58.0;
            let gb_w = 100.0f32;
            let gb_h = 30.0f32;
            let green_hover = mx >= gb_x && mx <= gb_x + gb_w && my >= gb_y && my <= gb_y + gb_h;
            if green_hover && clicked {
                team_green = Some(true);
                if player_name.len() >= 3 && password.len() >= 3 { break; }
            }
            let green_selected = team_green == Some(true);
            let green_fill = if green_selected { Color::from_rgba(34, 120, 34, 255) } else { Color::from_rgba(20, 60, 20, 255) };
            draw_rectangle(gb_x, gb_y, gb_w, gb_h, green_fill);
            draw_rectangle_lines(gb_x, gb_y, gb_w, gb_h, 2.0,
                if green_selected { Color::from_rgba(34, 197, 94, 255) } else if green_hover { GRAY } else { Color::from_rgba(60, 60, 60, 255) });
            let glabel = "Green";
            let gdim = measure_text(glabel, None, 20, 1.0);
            draw_text(glabel, gb_x + gb_w / 2.0 - gdim.width / 2.0, gb_y + 21.0, 20.0, Color::from_rgba(34, 197, 94, 255));

            // Red button
            let rb_x = sw / 2.0 + 50.0;
            let rb_y = sh / 2.0 + 58.0;
            let rb_w = 100.0f32;
            let rb_h = 30.0f32;
            let red_hover = mx >= rb_x && mx <= rb_x + rb_w && my >= rb_y && my <= rb_y + rb_h;
            if red_hover && clicked {
                team_green = Some(false);
                if player_name.len() >= 3 && password.len() >= 3 { break; }
            }
            let red_selected = team_green == Some(false);
            let red_fill = if red_selected { Color::from_rgba(120, 20, 20, 255) } else { Color::from_rgba(60, 10, 10, 255) };
            draw_rectangle(rb_x, rb_y, rb_w, rb_h, red_fill);
            draw_rectangle_lines(rb_x, rb_y, rb_w, rb_h, 2.0,
                if red_selected { Color::from_rgba(220, 38, 38, 255) } else if red_hover { GRAY } else { Color::from_rgba(60, 60, 60, 255) });
            let rlabel = "Red";
            let rdim = measure_text(rlabel, None, 20, 1.0);
            draw_text(rlabel, rb_x + rb_w / 2.0 - rdim.width / 2.0, rb_y + 21.0, 20.0, Color::from_rgba(220, 38, 38, 255));
        }

        let hint = if player_name.len() < 3 { "Name: minimum 3 characters" }
                   else if password.len() < 3 { "Password: minimum 3 characters" }
                   else if team_green.is_none() { "Select a team" }
                   else { "Press Enter to join" };
        let hdim = measure_text(hint, None, 18, 1.0);
        draw_text(hint, sw / 2.0 - hdim.width / 2.0, sh / 2.0 + 105.0, 18.0, GRAY);
        let tab_hint = "Tab: switch field";
        let thdim = measure_text(tab_hint, None, 16, 1.0);
        draw_text(tab_hint, sw / 2.0 - thdim.width / 2.0, sh / 2.0 + 128.0, 16.0, Color::from_rgba(100, 100, 100, 255));

        next_frame().await;
    }

    let is_green = team_green.unwrap_or(true);
    let my_color_rgb = if is_green { (34u8, 197u8, 94u8) } else { (220u8, 38u8, 38u8) };
    let team_name = if is_green { "Green" } else { "Red" };
    let mut game_state = GameState::new(player_name.clone(), my_color_rgb);
    // Pre-seed spawn position so the player isn't briefly visible at (0,0)
    game_state.my_pos = if is_green { (-2200.0, 2200.0) } else { (2200.0, -2200.0) };

    // ── Connect ──────────────────────────────────────────────────────────────
    // Native: WsClient::connect blocks until the TCP+WS handshake succeeds.
    // WASM:   connect() returns immediately; we poll connected() below.
    let mut ws = loop {
        match WsClient::connect(WS_URL) {
            Ok(ws) => break ws,
            Err(e) => {
                clear_background(BLACK);
                draw_text("Failed to connect to server", 20.0, 30.0, 24.0, RED);
                draw_text(&format!("{:?}", e), 20.0, 60.0, 18.0, BLACK);
                draw_text(
                    &format!("Retrying: {}", WS_URL),
                    20.0,
                    84.0,
                    18.0,
                    BLACK,
                );
                next_frame().await;
            }
        }
    };

    // Wait for WASM WebSocket handshake (native is already connected here)
    loop {
        if ws.connected() {
            break;
        }
        clear_background(BLACK);
        draw_text("Connecting to server...", 20.0, 30.0, 24.0, BLACK);
        next_frame().await;
    }

    send_msg(&ws, &ClientMessage::Join {
        name: player_name,
        password: password.clone(),
        team: team_name.to_string(),
    });

    // ── Wait for Welcome or Rejected ─────────────────────────────────────────
    'auth: loop {
        while let Some(data) = ws.try_recv() {
            if let Ok(msg) = bincode::deserialize::<ServerMessage>(&data) {
                match msg {
                    ServerMessage::Welcome { player_id } => {
                        game_state.my_id = Some(player_id);
                        break 'auth;
                    }
                    ServerMessage::Rejected { reason } => {
                        login_error = Some(reason);
                        continue 'login;
                    }
                    _ => {}
                }
            }
        }
        clear_background(BLACK);
        draw_text("Authenticating...", 20.0, 30.0, 24.0, BLACK);
        next_frame().await;
    }

    // ── Camera state ──────────────────────────────────────────────────────────
    // Start camera at the player's team spawn so they don't begin staring at (0,0).
    let (init_cam_x, init_cam_y) = if game_state.my_color == (34u8, 197u8, 94u8) {
        (-2200.0f32, 2200.0f32)   // Green spawn
    } else {
        (2200.0f32, -2200.0f32)   // Red spawn
    };
    let mut camera_x = init_cam_x;
    let mut camera_y = init_cam_y;
    let camera_pan_speed = 400.0f32;
    let edge_zone = 20i32;

    // Double-press key 1 → center camera. get_time() works on native + WASM.
    let mut key1_last_press: f64 = -10.0;
    let mut last_sent_time: f64 = 0.0;

    loop {
        let delta = get_frame_time();
        let now = get_time();
        let focused = is_app_focused();

        set_cursor_confinement(focused);
        show_mouse(true);

        // ── Receive messages ────────────────────────────────────────────────
        while let Some(data) = ws.try_recv() {
            if let Ok(msg) = bincode::deserialize::<ServerMessage>(&data) {
                match msg {
                    ServerMessage::WorldState { players, creeps } => {
                        // Sync own spawn position from the server-assigned value
                        if let Some(me) = players.iter().find(|p| Some(p.id) == game_state.my_id) {
                            game_state.my_pos = (me.x, me.y);
                        }
                        game_state.remote_players.clear();
                        for player in players {
                            if Some(player.id) != game_state.my_id {
                                game_state
                                    .remote_players
                                    .insert(player.id, RemotePlayer::new(player));
                            }
                        }
                        // Seed creep visuals from initial world state
                        game_state.creeps.clear();
                        for creep in creeps {
                            game_state.creeps.insert(creep.id, CreepVisual::new(creep));
                        }
                    }
                    ServerMessage::PlayerJoined { player } => {
                        if Some(player.id) != game_state.my_id {
                            game_state
                                .remote_players
                                .insert(player.id, RemotePlayer::new(player));
                        }
                    }
                    ServerMessage::PlayerMoved { player_id, x, y } => {
                        if Some(player_id) != game_state.my_id {
                            if let Some(remote) =
                                game_state.remote_players.get_mut(&player_id)
                            {
                                remote.update_target(x, y);
                                remote.player.x = x;
                                remote.player.y = y;
                            }
                        }
                    }
                    ServerMessage::PlayerLeft { player_id } => {
                        game_state.remote_players.remove(&player_id);
                    }
                    ServerMessage::CreepsState { creeps } => {
                        // Drop visuals for creeps the server no longer reports (dead)
                        let live_ids: std::collections::HashSet<u32> =
                            creeps.iter().map(|c| c.id).collect();
                        game_state.creeps.retain(|id, _| live_ids.contains(id));
                        for creep in creeps {
                            if let Some(cv) = game_state.creeps.get_mut(&creep.id) {
                                cv.update_target(creep.x, creep.y);
                                cv.creep.health = creep.health;
                                cv.creep.attack_charge = creep.attack_charge;
                            } else {
                                game_state.creeps.insert(creep.id, CreepVisual::new(creep));
                            }
                        }
                    }
                    ServerMessage::ProjectilesState { projectiles } => {
                        let live_ids: std::collections::HashSet<u32> =
                            projectiles.iter().map(|p| p.id).collect();
                        game_state.projectiles.retain(|id, _| live_ids.contains(id));
                        for proj in projectiles {
                            if let Some(pv) = game_state.projectiles.get_mut(&proj.id) {
                                pv.update_target(proj.x, proj.y);
                            } else {
                                game_state.projectiles.insert(proj.id, ProjectileVisual::new(proj));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // ── Double-press 1: center camera ───────────────────────────────────
        if is_key_pressed(KeyCode::Key1) {
            if now - key1_last_press < 0.4 {
                camera_x = game_state.my_pos.0;
                camera_y = game_state.my_pos.1;
                key1_last_press = -10.0;
            } else {
                key1_last_press = now;
            }
        }

        // ── Edge pan (normalized so diagonal == cardinal speed) ─────────────
        if focused {
            let (mx, my, phys_w, phys_h) = cursor_and_screen();
            let mut pdx = 0.0f32;
            let mut pdy = 0.0f32;
            if mx < edge_zone {
                pdx -= 1.0;
            }
            if mx > phys_w - edge_zone {
                pdx += 1.0;
            }
            if my < edge_zone {
                pdy -= 1.0;
            }
            if my > phys_h - edge_zone {
                pdy += 1.0;
            }
            if pdx != 0.0 || pdy != 0.0 {
                let len = (pdx * pdx + pdy * pdy).sqrt();
                camera_x += (pdx / len) * camera_pan_speed * delta;
                camera_y += (pdy / len) * camera_pan_speed * delta;
            }
        }

        camera_x = camera_x.clamp(-8192.0, 8192.0);
        camera_y = camera_y.clamp(-8192.0, 8192.0);

        // ── Player movement ─────────────────────────────────────────────────
        // sw/sh needed here for world-space mouse conversion
        let sw = screen_width();
        let sh = screen_height();

        let mut moved = false;

        // Right-click → attack a creep or move
        if is_mouse_button_pressed(MouseButton::Right) {
            let (mx, my) = mouse_position();
            // Camera2D with zoom=(2/sw, 2/sh): world = camera + (screen - center)
            let world_x = camera_x + (mx - sw * 0.5);
            let world_y = camera_y + (my - sh * 0.5);

            // Check if clicking on an enemy creep (circle hit test)
            let my_team = if game_state.my_color == (34u8, 197u8, 94u8) { "Green" } else { "Red" };
            let clicked_creep = game_state.creeps.values().find(|cv| {
                if cv.creep.team == my_team { return false; } // never attack own team
                let dx = world_x - cv.visual_x;
                let dy = world_y - cv.visual_y;
                let hit_r: f32 = if cv.creep.type_ == "melee" { 22.0 } else { 28.0 };
                (dx * dx + dy * dy) < hit_r * hit_r
            }).map(|cv| cv.creep.id);

            if let Some(creep_id) = clicked_creep {
                // Queue an attack: player must turn to face the target first.
                game_state.attack_target = Some(creep_id);
                game_state.move_target = None; // stop movement while turning
            } else {
                game_state.move_target = Some((world_x, world_y));
            }
        }

        // S key → stop movement and cancel pending attack
        if is_key_pressed(KeyCode::S) {
            game_state.move_target = None;
            game_state.attack_target = None;
        }

        // ── Turn-to-attack ──────────────────────────────────────────────────
        // Each frame we rotate the player toward the queued attack target.
        // Once the facing angle is within 15° of the target, fire and clear.
        if let Some(target_id) = game_state.attack_target {
            if let Some(cv) = game_state.creeps.get(&target_id) {
                let dx = cv.visual_x - game_state.my_pos.0;
                let dy = cv.visual_y - game_state.my_pos.1;
                let desired_angle = dy.atan2(dx);
                game_state.facing_angle =
                    dota_turn_toward(game_state.facing_angle, desired_angle, delta);
                let diff = angle_diff(game_state.facing_angle, desired_angle).abs();
                // Fire only once the turn is essentially complete (~2°).
                if diff < std::f32::consts::PI / 90.0 {
                    send_msg(&ws, &ClientMessage::Attack { creep_id: target_id });
                    game_state.attack_target = None;
                }
            } else {
                // Creep is dead or gone — cancel the pending attack.
                game_state.attack_target = None;
            }
        }

        // Move toward target (Invoker speed + turn rate)
        if let Some((tx, ty)) = game_state.move_target {
            let dx = tx - game_state.my_pos.0;
            let dy = ty - game_state.my_pos.1;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist > 1.0 {
                // Rotate facing angle toward movement direction (Dota 2 lerp model)
                let desired_angle = dy.atan2(dx);
                game_state.facing_angle = dota_turn_toward(
                    game_state.facing_angle, desired_angle, delta
                );

                // Move toward target while turning
                let step = INVOKER_SPEED * delta;
                if step >= dist {
                    game_state.my_pos.0 = tx;
                    game_state.my_pos.1 = ty;
                    game_state.move_target = None;
                } else {
                    game_state.my_pos.0 += (dx / dist) * step;
                    game_state.my_pos.1 += (dy / dist) * step;
                }
                moved = true;
            } else {
                game_state.move_target = None;
            }
        }

        game_state.my_pos.0 = game_state.my_pos.0.clamp(-8192.0, 8192.0);
        game_state.my_pos.1 = game_state.my_pos.1.clamp(-8192.0, 8192.0);

        for remote in game_state.remote_players.values_mut() {
            remote.interpolate(delta);
        }
        for cv in game_state.creeps.values_mut() {
            cv.interpolate(delta);
        }
        // Sword-flash detection: melee attack fires when charge wraps from high to low.
        for cv in game_state.creeps.values_mut() {
            if cv.creep.type_ == "melee"
                && cv.prev_charge > 0.75
                && cv.creep.attack_charge < 0.25
            {
                cv.sword_flash_timer = 0.25; // show sword square for 0.25 s
            }
            cv.prev_charge = cv.creep.attack_charge;
            if cv.sword_flash_timer > 0.0 {
                cv.sword_flash_timer = (cv.sword_flash_timer - delta).max(0.0);
            }
        }
        for pv in game_state.projectiles.values_mut() {
            pv.interpolate(delta);
        }

        if moved && (now - last_sent_time) > 0.033 {
            send_msg(
                &ws,
                &ClientMessage::Move {
                    x: game_state.my_pos.0,
                    y: game_state.my_pos.1,
                },
            );
            last_sent_time = now;
        }

        // ── Render ──────────────────────────────────────────────────────────
        // (sw / sh already computed above for movement)
        clear_background(Color::from_rgba(144, 238, 144, 255));

        set_camera(&Camera2D {
            target: vec2(camera_x, camera_y),
            zoom: vec2(2.0 / sw, 2.0 / sh),
            ..Default::default()
        });

        draw_rectangle_lines(-8192.0, -8192.0, 16384.0, 16384.0, 4.0, DARKGREEN);

        // Local player
        let my_color = Color::from_rgba(
            game_state.my_color.0,
            game_state.my_color.1,
            game_state.my_color.2,
            255,
        );
        draw_circle(game_state.my_pos.0, game_state.my_pos.1, 15.0, my_color);
        draw_circle_lines(
            game_state.my_pos.0,
            game_state.my_pos.1,
            20.0,
            2.0,
            WHITE,
        );
        // Facing direction indicator: line from circle edge to half-diameter beyond
        // Outer ring radius = 20, circle diameter = 40, half = 20 → tip at radius 40
        {
            let fx = game_state.facing_angle.cos();
            let fy = game_state.facing_angle.sin();
            let px = game_state.my_pos.0;
            let py = game_state.my_pos.1;
            draw_line(px + fx * 20.0, py + fy * 20.0,
                      px + fx * 40.0, py + fy * 40.0,
                      3.0, BLACK);
        }
        {
            let dim = measure_text(&game_state.my_name, None, 20, 1.0);
            draw_text(
                &game_state.my_name,
                game_state.my_pos.0 - dim.width / 2.0,
                game_state.my_pos.1 - 22.0,
                20.0,
                BLACK,
            );
        }

        // Creeps
        for cv in game_state.creeps.values() {
            let charge = cv.creep.attack_charge.clamp(0.0, 1.0);
            // Full-brightness team color — used for the uncharged portion and health bar.
            let color = if cv.creep.team == "Green" {
                Color::from_rgba(34, 197, 94, 255)
            } else {
                Color::from_rgba(220, 38, 38, 255)
            };
            // Near-black fill that grows left→right as charge builds.
            let dark = Color::from_rgba(15, 15, 15, 255);

            let (vx, vy) = (cv.visual_x, cv.visual_y);
            if cv.creep.type_ == "melee" {
                let half = 20.0f32;
                let full_w = half * 2.0;
                // Base shape in team color.
                draw_rectangle(vx - half, vy - half, full_w, full_w, color);
                // Dark charge fill from the left edge.
                let fill_w = charge * full_w;
                if fill_w > 0.5 {
                    draw_rectangle(vx - half, vy - half, fill_w, full_w, dark);
                }
                draw_rectangle_lines(vx - half, vy - half, full_w, full_w, 2.0,
                    Color::from_rgba(255, 255, 255, 180));

                // Sword flash: small bright square in the forward direction.
                if cv.sword_flash_timer > 0.0 {
                    let fwd_x: f32 = if cv.creep.team == "Green" {  std::f32::consts::FRAC_1_SQRT_2 }
                                     else                           { -std::f32::consts::FRAC_1_SQRT_2 };
                    let fwd_y: f32 = if cv.creep.team == "Green" { -std::f32::consts::FRAC_1_SQRT_2 }
                                     else                           {  std::f32::consts::FRAC_1_SQRT_2 };
                    let flash_frac = cv.sword_flash_timer / 0.25;
                    let alpha = (flash_frac * 240.0) as u8;
                    let sword_dist = 28.0_f32;
                    let sw2 = 10.0_f32;
                    let sx = vx + fwd_x * sword_dist - sw2 * 0.5;
                    let sy = vy + fwd_y * sword_dist - sw2 * 0.5;
                    draw_rectangle(sx, sy, sw2, sw2, Color::from_rgba(220, 220, 200, alpha));
                    draw_rectangle_lines(sx, sy, sw2, sw2, 1.5, Color::from_rgba(255, 255, 230, alpha));
                }
            } else {
                // Ranged: equilateral-ish triangle pointing up.
                let r = 28.0f32;
                let lx = vx - r * 0.866;   // leftmost x
                let rx_tri = vx + r * 0.866; // rightmost x
                let top_y = vy - r;
                let bot_y = vy + r * 0.5;

                let v1 = vec2(vx, top_y);
                let v2 = vec2(lx, bot_y);
                let v3 = vec2(rx_tri, bot_y);

                // Base triangle in team color.
                draw_triangle(v1, v2, v3, color);

                // Dark charge fill: left portion of the triangle clipped by a
                // vertical line at clip_x = lx + charge * (rx - lx).
                if charge > 0.001 {
                    let clip_x = lx + charge * (rx_tri - lx);
                    if clip_x <= vx {
                        // Left half only: the clipped shape is a triangle.
                        // Left edge goes from v2=(lx,bot_y) to v1=(vx,top_y).
                        let t = (clip_x - lx) / (vx - lx).max(0.001);
                        let y_edge = bot_y + t * (top_y - bot_y);
                        draw_triangle(v2, vec2(clip_x, y_edge), vec2(clip_x, bot_y), dark);
                    } else {
                        // Past the apex: clipped shape is a quad split into two triangles.
                        // Where clip_x intersects the right edge v1→v3:
                        let t = (clip_x - vx) / (rx_tri - vx).max(0.001);
                        let y_edge = top_y + t * (bot_y - top_y);
                        draw_triangle(v2, v1, vec2(clip_x, y_edge), dark);
                        draw_triangle(v2, vec2(clip_x, y_edge), vec2(clip_x, bot_y), dark);
                    }
                }
                draw_triangle_lines(v1, v2, v3, 2.0, Color::from_rgba(255, 255, 255, 180));
            }
            // Health bar
            let max_hp = creep_max_health(&cv.creep.type_);
            let hp_frac = (cv.creep.health / max_hp).clamp(0.0, 1.0);
            let bar_w = 40.0f32;
            let bar_h = 5.0f32;
            let bar_x = vx - bar_w * 0.5;
            let bar_y = vy - 32.0; // above the shape
            draw_rectangle(bar_x, bar_y, bar_w, bar_h, Color::from_rgba(0, 0, 0, 200));
            draw_rectangle(bar_x, bar_y, bar_w * hp_frac, bar_h, color);
        }

        // Projectiles
        for pv in game_state.projectiles.values() {
            let color = Color::from_rgba(pv.proj.color.0, pv.proj.color.1, pv.proj.color.2, 255);
            draw_circle(pv.visual_x, pv.visual_y, 5.0, color);
            draw_circle_lines(pv.visual_x, pv.visual_y, 5.0, 1.0, WHITE);
        }

        // Remote players
        for remote in game_state.remote_players.values() {
            let color = Color::from_rgba(
                remote.player.color.0,
                remote.player.color.1,
                remote.player.color.2,
                255,
            );
            draw_circle(remote.visual_x, remote.visual_y, 15.0, color);
            
            let dim = measure_text(&remote.player.name, None, 20, 1.0);
            draw_text(
                &remote.player.name,
                remote.visual_x - dim.width / 2.0,
                remote.visual_y - 22.0,
                20.0,
                BLACK,
            );
        }

        // Screen-space UI
        set_default_camera();
        draw_text(
            "Right-click: move | S: stop | Mouse edge: pan | Double-tap 1: center",
            10.0,
            20.0,
            18.0,
            BLACK,
        );
        let total = game_state.remote_players.len()
            + if game_state.my_id.is_some() { 1 } else { 0 };
        draw_text(
            &format!("Players online: {}", total),
            10.0,
            42.0,
            18.0,
            BLACK,
        );
        draw_text(&format!("Your name: {}", game_state.my_name), 10.0, 64.0, 18.0, WHITE);
        draw_text(
            &format!("Camera: ({:.0}, {:.0})", camera_x, camera_y),
            10.0,
            86.0,
            18.0,
            BLACK,
        );

        next_frame().await;
    } // end game loop

    } // end 'login loop
}