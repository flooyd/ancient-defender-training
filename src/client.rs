mod protocol;

use macroquad::prelude::*;
use protocol::*;
use std::collections::HashMap;

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

struct GameState {
    my_id: Option<u32>,
    my_name: String,
    remote_players: HashMap<u32, RemotePlayer>,
    my_pos: (f32, f32),
}

impl GameState {
    fn new(name: String) -> Self {
        GameState {
            my_id: None,
            my_name: name,
            remote_players: HashMap::new(),
            my_pos: (0.0, 0.0),
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

    // ── Login screen (name + password) ────────────────────────────────────────
    let mut focus_name = true;
    let mut player_name = String::new();
    let mut password = String::new();
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
            } else if !focus_name && player_name.len() >= 3 && password.len() >= 3 {
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
        draw_text(title, sw / 2.0 - tdim.width / 2.0, sh / 2.0 - 90.0, 32.0, WHITE);

        // Name field
        {
            let color = if focus_name { YELLOW } else { GRAY };
            draw_text("Name:", sw / 2.0 - 160.0, sh / 2.0 - 20.0, 20.0, color);
            let display = format!("{}{}", player_name, if focus_name { "_" } else { "" });
            draw_rectangle(sw / 2.0 - 60.0, sh / 2.0 - 42.0, 220.0, 30.0, Color::from_rgba(40, 40, 40, 255));
            draw_rectangle_lines(sw / 2.0 - 60.0, sh / 2.0 - 42.0, 220.0, 30.0, 2.0, color);
            draw_text(&display, sw / 2.0 - 50.0, sh / 2.0 - 20.0, 22.0, color);
        }

        // Password field
        {
            let color = if !focus_name { YELLOW } else { GRAY };
            draw_text("Password:", sw / 2.0 - 160.0, sh / 2.0 + 30.0, 20.0, color);
            let pw_display = format!("{}{}", "\u{25cf}".repeat(password.len()), if !focus_name { "_" } else { "" });
            draw_rectangle(sw / 2.0 - 60.0, sh / 2.0 + 8.0, 220.0, 30.0, Color::from_rgba(40, 40, 40, 255));
            draw_rectangle_lines(sw / 2.0 - 60.0, sh / 2.0 + 8.0, 220.0, 30.0, 2.0, color);
            draw_text(&pw_display, sw / 2.0 - 50.0, sh / 2.0 + 30.0, 22.0, color);
        }

        let hint = if player_name.len() < 3 { "Name: minimum 3 characters" }
                   else if password.len() < 3 { "Password: minimum 3 characters" }
                   else { "Press Enter to join" };
        let hdim = measure_text(hint, None, 18, 1.0);
        draw_text(hint, sw / 2.0 - hdim.width / 2.0, sh / 2.0 + 75.0, 18.0, GRAY);
        let tab_hint = "Tab: switch field";
        let thdim = measure_text(tab_hint, None, 16, 1.0);
        draw_text(tab_hint, sw / 2.0 - thdim.width / 2.0, sh / 2.0 + 98.0, 16.0, Color::from_rgba(100, 100, 100, 255));

        next_frame().await;
    }

    let mut game_state = GameState::new(player_name.clone());

    // ── Connect ──────────────────────────────────────────────────────────────
    // Native: WsClient::connect blocks until the TCP+WS handshake succeeds.
    // WASM:   connect() returns immediately; we poll connected() below.
    let mut ws = loop {
        match WsClient::connect(WS_URL) {
            Ok(ws) => break ws,
            Err(e) => {
                clear_background(BLACK);
                draw_text("Failed to connect to server", 20.0, 30.0, 24.0, RED);
                draw_text(&format!("{:?}", e), 20.0, 60.0, 18.0, WHITE);
                draw_text(
                    &format!("Retrying: {}", WS_URL),
                    20.0,
                    84.0,
                    18.0,
                    WHITE,
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
        draw_text("Connecting to server...", 20.0, 30.0, 24.0, WHITE);
        next_frame().await;
    }

    send_msg(&ws, &ClientMessage::Join {
        name: player_name,
        password: password.clone(),
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
        draw_text("Authenticating...", 20.0, 30.0, 24.0, WHITE);
        next_frame().await;
    }

    // ── Camera state ──────────────────────────────────────────────────────────
    let mut camera_x = 0.0f32;
    let mut camera_y = 0.0f32;
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
                    ServerMessage::WorldState { players } => {
                        game_state.remote_players.clear();
                        for player in players {
                            if Some(player.id) != game_state.my_id {
                                game_state
                                    .remote_players
                                    .insert(player.id, RemotePlayer::new(player));
                            }
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

        camera_x = camera_x.clamp(-1000.0, 1000.0);
        camera_y = camera_y.clamp(-1000.0, 1000.0);

        // ── Player movement ─────────────────────────────────────────────────
        let mut moved = false;
        let speed = 200.0 * delta;
        {
            let mut dx = 0.0f32;
            let mut dy = 0.0f32;
            if is_key_down(KeyCode::W) || is_key_down(KeyCode::Up) {
                dy -= 1.0;
            }
            if is_key_down(KeyCode::S) || is_key_down(KeyCode::Down) {
                dy += 1.0;
            }
            if is_key_down(KeyCode::A) || is_key_down(KeyCode::Left) {
                dx -= 1.0;
            }
            if is_key_down(KeyCode::D) || is_key_down(KeyCode::Right) {
                dx += 1.0;
            }
            if dx != 0.0 || dy != 0.0 {
                let len = (dx * dx + dy * dy).sqrt();
                game_state.my_pos.0 += (dx / len) * speed;
                game_state.my_pos.1 += (dy / len) * speed;
                moved = true;
            }
            game_state.my_pos.0 = game_state.my_pos.0.clamp(-980.0, 980.0);
            game_state.my_pos.1 = game_state.my_pos.1.clamp(-980.0, 980.0);
        }

        for remote in game_state.remote_players.values_mut() {
            remote.interpolate(delta);
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
        let sw = screen_width();
        let sh = screen_height();
        clear_background(Color::from_rgba(144, 238, 144, 255));

        set_camera(&Camera2D {
            target: vec2(camera_x, camera_y),
            zoom: vec2(2.0 / sw, 2.0 / sh),
            ..Default::default()
        });

        draw_rectangle_lines(-1000.0, -1000.0, 2000.0, 2000.0, 4.0, DARKGREEN);

        // Local player
        let my_color = if let Some(my_id) = game_state.my_id {
            Color::from_rgba(
                (my_id * 50 % 255) as u8,
                (my_id * 100 % 255) as u8,
                (my_id * 150 % 255) as u8,
                255,
            )
        } else {
            YELLOW
        };
        draw_circle(game_state.my_pos.0, game_state.my_pos.1, 15.0, my_color);
        draw_circle_lines(
            game_state.my_pos.0,
            game_state.my_pos.1,
            20.0,
            2.0,
            YELLOW,
        );
        {
            let dim = measure_text(&game_state.my_name, None, 20, 1.0);
            draw_text(
                &game_state.my_name,
                game_state.my_pos.0 - dim.width / 2.0,
                game_state.my_pos.1 - 22.0,
                20.0,
                WHITE,
            );
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
                WHITE,
            );
        }

        // Screen-space UI
        set_default_camera();
        draw_text(
            "WASD/Arrows: move | Mouse edge: pan | Double-tap 1: center",
            10.0,
            20.0,
            18.0,
            WHITE,
        );
        let total = game_state.remote_players.len()
            + if game_state.my_id.is_some() { 1 } else { 0 };
        draw_text(
            &format!("Players online: {}", total),
            10.0,
            42.0,
            18.0,
            WHITE,
        );
        draw_text(&format!("Your name: {}", game_state.my_name), 10.0, 64.0, 18.0, YELLOW);
        draw_text(
            &format!("Camera: ({:.0}, {:.0})", camera_x, camera_y),
            10.0,
            86.0,
            18.0,
            WHITE,
        );

        next_frame().await;
    } // end game loop

    } // end 'login loop
}