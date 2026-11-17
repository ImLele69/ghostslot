use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc::UnboundedSender;

use crate::protocol::{encode_frame, write_varint, Protocol};

const LOG_CAP: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    ToServer,
    ToClient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Pass,
    Delayed,
    Dropped,
    Injected,
    Info,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub at: Instant,
    pub dir: Option<Dir>,
    pub tag: Tag,
    pub text: String,
}

pub enum Out {
    Frame(Vec<u8>),
    EnableCipher([u8; 16]),
    Close,
}

#[derive(Debug, Clone, Default)]
pub struct Screen {
    pub id: i32,
    pub kind: i32,
    pub title: String,
}

pub struct Delayed {
    pub desc: String,
    pub frame: Vec<u8>,
}

pub struct Link {
    pub proto: Protocol,
    pub threshold: Arc<AtomicI32>,
    pub to_server: UnboundedSender<Out>,
    pub to_client: UnboundedSender<Out>,
    pub cb_play: bool,
    pub sb_play: bool,
}

#[derive(Default)]
pub struct Session {
    pub link: Option<Link>,
    pub status: String,
    pub client_screen: Option<Screen>,
    pub server_container: Option<i32>,
    pub state_id: i32,
    pub drop_packets: bool,
    pub delay_packets: bool,
    pub delayed: Vec<Delayed>,
    pub saved_open: Option<Vec<u8>>,
    pub saved_content: Option<Vec<u8>>,
    pub saved_label: String,
    last_open: Option<Vec<u8>>,
    last_content: Option<(i32, Vec<u8>)>,
    pub log: VecDeque<LogLine>,
    pub log_all: bool,
    pub echo: bool,
    pub chunks: BTreeSet<(i32, i32)>,
}

pub type Shared = Arc<Mutex<Session>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickMode {
    Pickup,
    QuickMove,
    Swap,
    Clone,
    Throw,
    QuickCraft,
    PickupAll,
}

impl ClickMode {
    pub const ALL: [ClickMode; 7] = [
        ClickMode::Pickup,
        ClickMode::QuickMove,
        ClickMode::Swap,
        ClickMode::Clone,
        ClickMode::Throw,
        ClickMode::QuickCraft,
        ClickMode::PickupAll,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ClickMode::Pickup => "PICKUP",
            ClickMode::QuickMove => "QUICK_MOVE",
            ClickMode::Swap => "SWAP",
            ClickMode::Clone => "CLONE",
            ClickMode::Throw => "THROW",
            ClickMode::QuickCraft => "QUICK_CRAFT",
            ClickMode::PickupAll => "PICKUP_ALL",
        }
    }

    pub fn from_id(id: i32) -> String {
        Self::ALL
            .get(id as usize)
            .map(|m| m.name().to_string())
            .unwrap_or_else(|| format!("mode#{id}"))
    }
}

impl Session {
    pub fn log(&mut self, dir: Option<Dir>, tag: Tag, text: impl Into<String>) {
        let text = text.into();
        if self.echo {
            let d = match dir {
                Some(Dir::ToServer) => "C->S",
                Some(Dir::ToClient) => "S->C",
                None => "    ",
            };
            eprintln!("{d} {tag:?} {text}");
        }
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(LogLine {
            at: Instant::now(),
            dir,
            tag,
            text,
        });
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.log(None, Tag::Info, text);
    }

    pub fn reset_connection(&mut self) {
        self.link = None;
        self.client_screen = None;
        self.server_container = None;
        self.state_id = 0;
        self.delayed.clear();
        self.chunks.clear();
        self.last_open = None;
        self.last_content = None;
    }

    pub fn in_play(&self) -> bool {
        self.link.as_ref().is_some_and(|l| l.cb_play && l.sb_play)
    }

    pub fn on_open_screen(&mut self, screen: Screen, payload: Vec<u8>) {
        self.server_container = Some(screen.id);
        self.client_screen = Some(screen);
        self.last_open = Some(payload);
        self.last_content = None;
    }

    // only the container the server considers open moves the revision. the
    // player inventory (#0) counts when nothing else is open, the cursor and
    // inventory pseudo windows (-1, -2) never do
    pub fn track_revision(&mut self, window: i32, state_id: i32) {
        if window == self.server_container.unwrap_or(0) {
            self.state_id = state_id;
        }
    }

    pub fn on_set_content(&mut self, window: i32, state_id: i32, payload: Vec<u8>) {
        self.track_revision(window, state_id);
        if self.client_screen.as_ref().is_some_and(|s| s.id == window) {
            self.last_content = Some((window, payload));
        }
    }

    fn send(&mut self, dir: Dir, id: i32, payload: &[u8], desc: String) -> bool {
        let Some(link) = &self.link else {
            return false;
        };
        let frame = encode_frame(id, payload, link.threshold.load(Ordering::SeqCst));
        let tx = match dir {
            Dir::ToServer => &link.to_server,
            Dir::ToClient => &link.to_client,
        };
        let ok = tx.send(Out::Frame(frame)).is_ok();
        self.log(Some(dir), Tag::Injected, desc);
        ok
    }

    pub fn close_without_packet(&mut self) {
        let Some(link) = &self.link else { return };
        let Some(screen) = self.client_screen.take() else {
            self.info("no GUI open on the client");
            return;
        };
        let (proto, id) = (link.proto.clone(), link.proto.cb_container_close);
        let mut p = Vec::new();
        proto.write_container_id(&mut p, screen.id);
        self.send(Dir::ToClient, id, &p, format!("container_close #{} (client only)", screen.id));
    }

    pub fn desync(&mut self) {
        let Some(link) = &self.link else { return };
        let Some(window) = self.server_container.take() else {
            self.info("the server has no container open");
            return;
        };
        let (proto, id) = (link.proto.clone(), link.proto.sb_container_close);
        let mut p = Vec::new();
        proto.write_container_id(&mut p, window);
        self.send(Dir::ToServer, id, &p, format!("container_close #{window} (server only)"));
    }

    pub fn unload_chunks(&mut self) {
        let Some(link) = &self.link else { return };
        let (proto, id) = (link.proto.clone(), link.proto.cb_forget_chunk);
        if self.chunks.is_empty() {
            self.info("no loaded chunks to unload");
            return;
        }
        let chunks = std::mem::take(&mut self.chunks);
        let n = chunks.len();
        for (x, z) in chunks {
            let zx = proto.version >= crate::protocol::PROTO_CHUNK_ZX;
            let (a, b) = if zx { (z, x) } else { (x, z) };
            let mut p = Vec::with_capacity(8);
            p.extend_from_slice(&a.to_be_bytes());
            p.extend_from_slice(&b.to_be_bytes());
            let Some(link) = &self.link else { return };
            let frame = encode_frame(id, &p, link.threshold.load(Ordering::SeqCst));
            let _ = link.to_client.send(Out::Frame(frame));
        }
        let desc = format!("forget_level_chunk x{n} (client only)");
        self.log(Some(Dir::ToClient), Tag::Injected, desc);
    }

    pub fn flush_delayed(&mut self) -> usize {
        let Some(link) = &self.link else { return 0 };
        let tx = link.to_server.clone();
        let items: Vec<_> = self.delayed.drain(..).collect();
        let n = items.len();
        for d in items {
            let _ = tx.send(Out::Frame(d.frame));
            self.log(Some(Dir::ToServer), Tag::Injected, format!("release: {}", d.desc));
        }
        n
    }

    pub fn disconnect_and_send(&mut self) {
        let n = self.flush_delayed();
        if let Some(link) = &self.link {
            let _ = link.to_server.send(Out::Close);
            self.info(format!("disconnecting after {n} packets"));
        }
    }

    pub fn save_gui(&mut self) {
        match (&self.last_open, &self.client_screen) {
            (Some(open), Some(screen)) => {
                self.saved_open = Some(open.clone());
                self.saved_content = self.last_content.as_ref().map(|(_, c)| c.clone());
                self.saved_label = format!("#{} {}", screen.id, screen.title);
                let label = self.saved_label.clone();
                self.info(format!("GUI saved: {label}"));
            }
            _ => self.info("no GUI to save"),
        }
    }

    pub fn load_gui(&mut self) {
        let Some(link) = &self.link else { return };
        let Some(open) = self.saved_open.clone() else {
            self.info("no saved GUI");
            return;
        };
        let (open_id, content_id) = (link.proto.cb_open_screen, link.proto.cb_set_content);
        let proto = link.proto.clone();
        if let Ok(screen) = crate::proxy::parse_open_screen(&proto, &open) {
            self.client_screen = Some(screen);
        }
        let desc = format!("open_screen {} (restore)", self.saved_label);
        self.send(Dir::ToClient, open_id, &open, desc);
        if let Some(content) = self.saved_content.clone() {
            let desc = "container_set_content (restore)".to_string();
            self.send(Dir::ToClient, content_id, &content, desc);
        }
    }

    pub fn fabricate_click(
        &mut self,
        window: i32,
        state_id: i32,
        slot: i16,
        button: i8,
        mode: ClickMode,
        times: u32,
        delay: bool,
    ) {
        let Some(link) = &self.link else { return };
        let proto = link.proto.clone();
        let id = proto.sb_container_click;
        let mut p = Vec::new();
        proto.write_container_id(&mut p, window);
        write_varint(&mut p, state_id);
        p.extend_from_slice(&slot.to_be_bytes());
        p.push(button as u8);
        write_varint(&mut p, mode as i32);
        write_varint(&mut p, 0);
        // empty carried item: bool false (<1.20.5), count 0 (1.20.5-1.21.4) and
        // an absent HashedStack (1.21.5+) all encode as a single zero byte. three format
        // changes and one byte survived all of them. I'll take it
        p.push(0);
        let desc = format!(
            "container_click #{window} rev={state_id} slot={slot} btn={button} {}",
            mode.name()
        );
        self.emit_fabricated(id, &p, desc, times, delay);
    }

    pub fn fabricate_button(&mut self, window: i32, button: i32, times: u32, delay: bool) {
        let Some(link) = &self.link else { return };
        let proto = link.proto.clone();
        let id = proto.sb_button_click;
        let mut p = Vec::new();
        if proto.varint_container() {
            write_varint(&mut p, window);
            write_varint(&mut p, button);
        } else {
            p.push(window as u8);
            p.push(button as u8);
        }
        let desc = format!("container_button_click #{window} btn={button}");
        self.emit_fabricated(id, &p, desc, times, delay);
    }

    fn emit_fabricated(&mut self, id: i32, payload: &[u8], desc: String, times: u32, delay: bool) {
        if !delay {
            for _ in 0..times {
                self.send(Dir::ToServer, id, payload, desc.clone());
            }
            return;
        }
        let Some(link) = &self.link else { return };
        let frame = encode_frame(id, payload, link.threshold.load(Ordering::SeqCst));
        let desc = format!("{desc} (fabricated)");
        for _ in 0..times {
            self.delayed.push(Delayed { desc: desc.clone(), frame: frame.clone() });
            self.log(Some(Dir::ToServer), Tag::Delayed, desc.clone());
        }
    }
}
