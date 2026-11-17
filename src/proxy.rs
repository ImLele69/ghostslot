use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinSet;

use crate::auth::{self, Account, Cfb8};
use crate::protocol::*;
use crate::state::*;

const LOGIN: u8 = 0;
const CONFIG: u8 = 1;
const PLAY: u8 = 2;

#[derive(Clone)]
pub struct Config {
    pub listen_port: u16,
    pub target: String,
    pub account: Option<Account>,
}

struct FrameReader {
    rd: OwnedReadHalf,
    buf: Vec<u8>,
    start: usize,
    cipher: Option<Cfb8>,
}

impl FrameReader {
    fn new(rd: OwnedReadHalf) -> Self {
        FrameReader { rd, buf: Vec::new(), start: 0, cipher: None }
    }

    fn enable_cipher(&mut self, key: &[u8; 16]) {
        let mut c = Cfb8::decryptor(key);
        c.apply(&mut self.buf[self.start..]);
        self.cipher = Some(c);
    }

    fn leftover(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    // frame body without the length prefix, None on EOF
    async fn next(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let avail = &self.buf[self.start..];
            let mut r = Reader::new(avail);
            if let Ok(len) = r.varint() {
                // vanilla caps frames at 2 MiB. anything bigger, I'm not reading it
                if !(0..=2 * 1024 * 1024).contains(&len) {
                    bail!("frame with a bogus length: {len}");
                }
                let header = avail.len() - r.remaining().len();
                if r.remaining().len() >= len as usize {
                    let body = r.remaining()[..len as usize].to_vec();
                    self.start += header + len as usize;
                    if self.start > 64 * 1024 && self.start * 2 > self.buf.len() {
                        self.buf.drain(..self.start);
                        self.start = 0;
                    }
                    return Ok(Some(body));
                }
            } else if avail.len() >= 5 {
                bail!("invalid length varint");
            }
            let mut tmp = [0u8; 32 * 1024];
            let n = self.rd.read(&mut tmp).await?;
            if n == 0 {
                return Ok(None);
            }
            if let Some(c) = &mut self.cipher {
                c.apply(&mut tmp[..n]);
            }
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }
}

async fn writer_task(mut wr: OwnedWriteHalf, mut rx: UnboundedReceiver<Out>) {
    let mut cipher: Option<Cfb8> = None;
    while let Some(msg) = rx.recv().await {
        match msg {
            Out::Frame(mut bytes) => {
                if let Some(c) = &mut cipher {
                    c.apply(&mut bytes);
                }
                if wr.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            Out::EnableCipher(key) => cipher = Some(Cfb8::encryptor(&key)),
            Out::Close => break,
        }
    }
    let _ = wr.shutdown().await;
}

pub async fn run(cfg: Config, shared: Shared) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", cfg.listen_port))
        .await
        .with_context(|| format!("is port {} already in use?", cfg.listen_port))?;
    shared.lock().unwrap().info(format!(
        "listening on 127.0.0.1:{}, forwarding to {} (versions {})",
        cfg.listen_port,
        cfg.target,
        supported_range()
    ));
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (sock, _) = accepted?;
                let _ = sock.set_nodelay(true);
                let (cfg, shared) = (cfg.clone(), shared.clone());
                sessions.spawn(async move {
                    if let Err(e) = handle(sock, cfg, shared.clone()).await {
                        shared.lock().unwrap().info(format!("connection closed: {e:#}"));
                    }
                });
            }
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
        }
    }
}

struct Handshake {
    protocol: i32,
    host: String,
    next: i32,
}

fn parse_handshake(body: &[u8]) -> Result<Handshake> {
    let (id, payload) = decode_frame(body, -1)?;
    if id != 0 {
        bail!("first packet is not a handshake");
    }
    let mut r = Reader::new(&payload);
    let protocol = r.varint()?;
    let host = r.string()?;
    let _port = r.u16()?;
    let next = r.varint()?;
    Ok(Handshake { protocol, host, next })
}

fn build_handshake(hs: &Handshake, host: &str, port: u16) -> Vec<u8> {
    // Forge and NeoForge append "\0FML3\0" style markers to the host, keep them
    let suffix = hs.host.find('\0').map(|i| &hs.host[i..]).unwrap_or("");
    let mut p = Vec::new();
    write_varint(&mut p, hs.protocol);
    write_string(&mut p, &format!("{host}{suffix}"));
    p.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut p, hs.next);
    encode_frame(0, &p, -1)
}

async fn resolve(target: &str) -> Result<(String, u16)> {
    if let Some((h, p)) = target.rsplit_once(':') {
        if let Ok(port) = p.parse() {
            return Ok((h.to_string(), port));
        }
    }
    if let Ok(resolver) = hickory_resolver::Resolver::builder_tokio().map(|b| b.build()) {
        if let Ok(resolver) = resolver {
            if let Ok(srv) = resolver.srv_lookup(format!("_minecraft._tcp.{target}.")).await {
                if let Some(rec) = srv.answers().iter().find_map(|r| match &r.data {
                    hickory_resolver::proto::rr::RData::SRV(s) => Some(s.clone()),
                    _ => None,
                }) {
                    let host = rec.target.to_utf8().trim_end_matches('.').to_string();
                    return Ok((host, rec.port));
                }
            }
        }
    }
    Ok((target.to_string(), 25565))
}

async fn send_login_disconnect(sock: &mut TcpStream, msg: &str) {
    let mut p = Vec::new();
    write_string(&mut p, &serde_json::json!({ "text": msg }).to_string());
    let _ = sock.write_all(&encode_frame(0, &p, -1)).await;
    let _ = sock.shutdown().await;
}

async fn handle(mut client: TcpStream, cfg: Config, shared: Shared) -> Result<()> {
    let (crd, cwr) = client.into_split();
    let mut creader = FrameReader::new(crd);
    let Some(body) = creader.next().await? else { return Ok(()) };
    let hs = parse_handshake(&body)?;
    let (host, port) = resolve(&cfg.target).await?;

    if hs.next == 1 {
        let server = TcpStream::connect((host.as_str(), port)).await?;
        let (mut srd, mut swr) = server.into_split();
        swr.write_all(&build_handshake(&hs, &host, port)).await?;
        swr.write_all(creader.leftover()).await?;
        let mut crd = creader.rd;
        let mut cwr = cwr;
        tokio::select! {
            _ = tokio::io::copy(&mut crd, &mut swr) => {}
            _ = tokio::io::copy(&mut srd, &mut cwr) => {}
        }
        return Ok(());
    }

    let Some(proto) = Protocol::get(hs.protocol) else {
        client = creader.rd.reunite(cwr)?;
        let msg = format!(
            "ghostslot: protocol {} is not supported ({})",
            hs.protocol,
            supported_range()
        );
        send_login_disconnect(&mut client, &msg).await;
        bail!(msg);
    };

    if shared.lock().unwrap().link.is_some() {
        client = creader.rd.reunite(cwr)?;
        send_login_disconnect(&mut client, "ghostslot: another player is already connected").await;
        bail!("second connection refused");
    }

    let server = TcpStream::connect((host.as_str(), port))
        .await
        .with_context(|| format!("connecting to {host}:{port}"))?;
    let _ = server.set_nodelay(true);
    let (srd, swr) = server.into_split();
    let mut sreader = FrameReader::new(srd);

    let (to_server, srx) = unbounded_channel();
    let (to_client, crx) = unbounded_channel();
    let w1 = tokio::spawn(writer_task(swr, srx));
    let w2 = tokio::spawn(writer_task(cwr, crx));

    to_server.send(Out::Frame(build_handshake(&hs, &host, port)))?;

    let threshold = Arc::new(AtomicI32::new(-1));
    let sb_state = Arc::new(AtomicU8::new(LOGIN));
    {
        let mut s = shared.lock().unwrap();
        s.reset_connection();
        s.link = Some(Link {
            proto: proto.clone(),
            threshold: threshold.clone(),
            to_server: to_server.clone(),
            to_client: to_client.clone(),
            cb_play: false,
            sb_play: false,
        });
        s.status = format!("{} (protocol {}) via {host}:{port}", proto.label, proto.version);
        s.info(format!("client connected, {}", proto.label));
    }

    let ctx = Ctx { proto, threshold, sb_state, shared: shared.clone(), to_server, to_client };
    let result = tokio::select! {
        r = server_to_client(&ctx, &mut sreader, cfg.account.as_ref()) => r,
        r = client_to_server(&ctx, &mut creader) => r,
    };
    let _ = ctx.to_server.send(Out::Close);
    let _ = ctx.to_client.send(Out::Close);
    let _ = tokio::join!(w1, w2);
    {
        let mut s = shared.lock().unwrap();
        s.reset_connection();
        s.status.clear();
    }
    result
}

struct Ctx {
    proto: Protocol,
    threshold: Arc<AtomicI32>,
    sb_state: Arc<AtomicU8>,
    shared: Shared,
    to_server: UnboundedSender<Out>,
    to_client: UnboundedSender<Out>,
}

impl Ctx {
    fn set_play(&self, cb: Option<bool>, sb: Option<bool>) {
        let mut s = self.shared.lock().unwrap();
        if let Some(link) = &mut s.link {
            if let Some(v) = cb {
                link.cb_play = v;
            }
            if let Some(v) = sb {
                link.sb_play = v;
            }
        }
    }
}

async fn server_to_client(
    ctx: &Ctx,
    reader: &mut FrameReader,
    account: Option<&Account>,
) -> Result<()> {
    let p = &ctx.proto;
    let mut state = LOGIN;
    while let Some(body) = reader.next().await? {
        let (id, payload) = decode_frame(&body, ctx.threshold.load(Ordering::SeqCst))?;
        match state {
            LOGIN => match id {
                1 => {
                    let key = encryption(ctx, &payload, account).await?;
                    reader.enable_cipher(&key);
                    continue;
                }
                2 => {
                    if p.has_config_phase() {
                        state = CONFIG;
                    } else {
                        state = PLAY;
                        ctx.sb_state.store(PLAY, Ordering::SeqCst);
                        ctx.set_play(Some(true), Some(true));
                    }
                }
                3 => {
                    let t = Reader::new(&payload).varint()?;
                    ctx.threshold.store(t, Ordering::SeqCst);
                }
                0 => {
                    let reason = Reader::new(&payload).string().unwrap_or_default();
                    ctx.shared.lock().unwrap().info(format!("login refused: {reason}"));
                }
                _ => {}
            },
            CONFIG => {
                if id == p.cfg_cb_finish {
                    state = PLAY;
                    ctx.set_play(Some(true), None);
                }
            }
            _ => {
                if id == p.cb_start_configuration {
                    state = CONFIG;
                    ctx.set_play(Some(false), None);
                } else {
                    on_clientbound(ctx, id, &payload);
                }
            }
        }
        ctx.to_client.send(Out::Frame(wrap_body(&body)))?;
    }
    Ok(())
}

async fn encryption(ctx: &Ctx, payload: &[u8], account: Option<&Account>) -> Result<[u8; 16]> {
    use rsa::pkcs8::DecodePublicKey;

    let mut r = Reader::new(payload);
    let server_id = r.string()?;
    let pubkey = r.byte_array()?.to_vec();
    let verify = r.byte_array()?.to_vec();
    let should_auth = if ctx.proto.version >= PROTO_SHOULD_AUTH { r.bool()? } else { true };

    let secret: [u8; 16] = rand::random();
    if should_auth {
        let account = account
            .context("the server is in online mode, pick an account")?
            .clone();
        let hash = auth::server_hash(&server_id, &secret, &pubkey);
        tokio::task::spawn_blocking(move || auth::session_join(&account, &hash)).await??;
    }

    let key = rsa::PublicKey::from_public_key_der(&pubkey).context("server RSA key")?;
    let mut rng = rand::rng();
    let enc_secret = key.encrypt(&mut rng, rsa::Pkcs1v15Encrypt, &secret)?;
    let enc_verify = key.encrypt(&mut rng, rsa::Pkcs1v15Encrypt, &verify)?;

    let mut p = Vec::new();
    write_byte_array(&mut p, &enc_secret);
    if ctx.proto.version <= PROTO_VERIFY_TOKEN_FLAG_MAX {
        p.push(1);
    }
    write_byte_array(&mut p, &enc_verify);
    ctx.to_server.send(Out::Frame(encode_frame(1, &p, ctx.threshold.load(Ordering::SeqCst))))?;
    ctx.to_server.send(Out::EnableCipher(secret))?;
    ctx.shared.lock().unwrap().info(if should_auth {
        "encryption on, session authenticated"
    } else {
        "encryption on"
    });
    Ok(secret)
}

pub fn parse_open_screen(p: &Protocol, payload: &[u8]) -> Result<Screen> {
    let mut r = Reader::new(payload);
    let id = r.varint()?;
    let kind = r.varint()?;
    let title = read_title(&mut r, p);
    Ok(Screen { id, kind, title })
}

fn on_clientbound(ctx: &Ctx, id: i32, payload: &[u8]) {
    let p = &ctx.proto;
    let mut s = ctx.shared.lock().unwrap();
    let mut r = Reader::new(payload);
    let line = if id == p.cb_open_screen {
        parse_open_screen(p, payload).ok().map(|screen| {
            let line =
                format!("open_screen #{} type={} \"{}\"", screen.id, screen.kind, screen.title);
            s.on_open_screen(screen, payload.to_vec());
            line
        })
    } else if id == p.cb_set_content {
        (|| -> Result<String> {
            let w = p.read_container_id(&mut r)?;
            let rev = r.varint()?;
            let n = r.varint()?;
            s.on_set_content(w, rev, payload.to_vec());
            Ok(format!("container_set_content #{w} rev={rev} slot={n}"))
        })()
        .ok()
    } else if id == p.cb_set_slot {
        (|| -> Result<String> {
            let w = if p.varint_container() { r.varint()? } else { r.u8()? as i8 as i32 };
            let rev = r.varint()?;
            let slot = r.i16()?;
            s.track_revision(w, rev);
            Ok(format!("container_set_slot #{w} rev={rev} slot={slot}"))
        })()
        .ok()
    } else if id == p.cb_container_close {
        p.read_container_id(&mut r).ok().map(|w| {
            s.client_screen = None;
            s.server_container = None;
            format!("container_close #{w}")
        })
    } else if id == p.cb_set_data {
        (|| -> Result<String> {
            let w = p.read_container_id(&mut r)?;
            let prop = r.i16()?;
            let val = r.i16()?;
            Ok(format!("container_set_data #{w} prop={prop} val={val}"))
        })()
        .ok()
    } else if id == p.cb_level_chunk {
        if let (Ok(x), Ok(z)) = (r.i32(), r.i32()) {
            s.chunks.insert((x, z));
        }
        None
    } else if id == p.cb_forget_chunk {
        if let (Ok(a), Ok(b)) = (r.i32(), r.i32()) {
            let (x, z) = if p.version >= PROTO_CHUNK_ZX { (b, a) } else { (a, b) };
            s.chunks.remove(&(x, z));
        }
        None
    } else if id == p.cb_disconnect {
        Some("disconnect".to_string())
    } else if s.log_all {
        Some(format!("0x{id:02x} ({} bytes)", payload.len()))
    } else {
        None
    };
    if let Some(line) = line {
        s.log(Some(Dir::ToClient), Tag::Pass, line);
    }
}

async fn client_to_server(ctx: &Ctx, reader: &mut FrameReader) -> Result<()> {
    let p = &ctx.proto;
    while let Some(body) = reader.next().await? {
        let (id, payload) = decode_frame(&body, ctx.threshold.load(Ordering::SeqCst))?;
        match ctx.sb_state.load(Ordering::SeqCst) {
            LOGIN => {
                if p.has_config_phase() && id == 3 {
                    ctx.sb_state.store(CONFIG, Ordering::SeqCst);
                }
            }
            CONFIG => {
                if id == p.cfg_sb_finish {
                    ctx.sb_state.store(PLAY, Ordering::SeqCst);
                    ctx.set_play(None, Some(true));
                }
            }
            _ => {
                if id == p.sb_configuration_ack {
                    ctx.sb_state.store(CONFIG, Ordering::SeqCst);
                    ctx.set_play(None, Some(false));
                } else if !on_serverbound(ctx, id, &payload, &body) {
                    continue;
                }
            }
        }
        ctx.to_server.send(Out::Frame(wrap_body(&body)))?;
    }
    Ok(())
}

// false when the packet was held back instead of forwarded
fn on_serverbound(ctx: &Ctx, id: i32, payload: &[u8], body: &[u8]) -> bool {
    let p = &ctx.proto;
    let mut s = ctx.shared.lock().unwrap();
    let mut r = Reader::new(payload);
    let line = if id == p.sb_container_click {
        (|| -> Result<String> {
            let w = p.read_container_id(&mut r)?;
            let rev = r.varint()?;
            let slot = r.i16()?;
            let button = r.u8()? as i8;
            let mode = r.varint()?;
            let changed = r.varint()?;
            Ok(format!(
                "container_click #{w} rev={rev} slot={slot} btn={button} {} changed={changed}",
                ClickMode::from_id(mode)
            ))
        })()
        .ok()
        .map(|l| (l, true))
    } else if id == p.sb_button_click {
        (|| -> Result<String> {
            let (w, b) = if p.varint_container() {
                (r.varint()?, r.varint()?)
            } else {
                (r.u8()? as i8 as i32, r.u8()? as i8 as i32)
            };
            Ok(format!("container_button_click #{w} btn={b}"))
        })()
        .ok()
        .map(|l| (l, true))
    } else if id == p.sb_container_close {
        p.read_container_id(&mut r).ok().map(|w| {
            if s.server_container == Some(w) {
                s.server_container = None;
            }
            if s.client_screen.as_ref().is_some_and(|c| c.id == w) {
                s.client_screen = None;
            }
            (format!("container_close #{w}"), false)
        })
    } else if s.log_all {
        Some((format!("0x{id:02x} ({} bytes)", payload.len()), false))
    } else {
        None
    };

    match line {
        Some((line, delayable)) if delayable && s.drop_packets => {
            s.log(Some(Dir::ToServer), Tag::Dropped, line);
            false
        }
        Some((line, delayable)) if delayable && s.delay_packets => {
            s.delayed.push(Delayed { desc: line.clone(), frame: wrap_body(body) });
            s.log(Some(Dir::ToServer), Tag::Delayed, line);
            false
        }
        Some((line, _)) => {
            s.log(Some(Dir::ToServer), Tag::Pass, line);
            true
        }
        None => true,
    }
}
