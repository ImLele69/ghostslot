use std::io::BufRead;

use crate::state::{ClickMode, Session, Shared};

pub const HELP: &str = "\
commands:
  close                    close without packet
  desync                   close the container on the server only
  save | load              save / restore the GUI
  unload                   unload chunks on the client (the server is not told)
  send on|off              off drops the client's clicks
  delay on|off             on holds the client's clicks in the queue
  flush | discard          send / discard the queue
  disconnect               send the queue and close the connection
  click <win|-> <rev|-> <slot> <btn> <MODE> [times] [queue]
  button <win|-> <id> [times] [queue]
  status
'-' uses the current value";

pub fn spawn_stdin(shared: Shared) {
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let mut s = shared.lock().unwrap();
            if let Err(e) = run(&mut s, &line) {
                s.info(format!("invalid command: {e}"));
            }
        }
    });
}

fn num<T: std::str::FromStr>(arg: Option<&str>, current: T) -> Result<T, String> {
    match arg {
        None | Some("-") => Ok(current),
        Some(v) => v.parse().map_err(|_| format!("invalid number: {v}")),
    }
}

pub fn run(s: &mut Session, line: &str) -> Result<(), String> {
    let mut it = line.split_whitespace();
    let Some(cmd) = it.next() else { return Ok(()) };
    let window = s.client_screen.as_ref().map(|c| c.id).unwrap_or(0);
    match cmd {
        "close" => s.close_without_packet(),
        "desync" => s.desync(),
        "save" => s.save_gui(),
        "load" => s.load_gui(),
        "unload" => s.unload_chunks(),
        "delay" => {
            s.delay_packets = it.next() != Some("off");
            let on = s.delay_packets;
            s.info(format!("delay packets: {}", if on { "on" } else { "off" }));
        }
        "send" => {
            s.drop_packets = it.next() == Some("off");
            let off = s.drop_packets;
            s.info(format!("send packets: {}", if off { "off" } else { "on" }));
        }
        "flush" => {
            s.flush_delayed();
        }
        "discard" => {
            s.delayed.clear();
            s.info("queue discarded");
        }
        "disconnect" => s.disconnect_and_send(),
        "click" => {
            let w = num(it.next(), window)?;
            let rev = num(it.next(), s.state_id)?;
            let slot = num(it.next(), 0i16)?;
            let btn = num(it.next(), 0i8)?;
            let mode = it.next().unwrap_or("PICKUP");
            let mode = ClickMode::ALL
                .into_iter()
                .find(|m| m.name().eq_ignore_ascii_case(mode))
                .ok_or(format!("unknown mode: {mode}"))?;
            let times = num(it.next(), 1u32)?.clamp(1, 500);
            let delay = it.next() == Some("queue");
            s.fabricate_click(w, rev, slot, btn, mode, times, delay);
        }
        "button" => {
            let w = num(it.next(), window)?;
            let id = num(it.next(), 0i32)?;
            let times = num(it.next(), 1u32)?.clamp(1, 500);
            let delay = it.next() == Some("queue");
            s.fabricate_button(w, id, times, delay);
        }
        "status" => {
            let msg = format!(
                "client={:?} server={:?} rev={} queue={} chunks={}",
                s.client_screen.as_ref().map(|c| c.id),
                s.server_container,
                s.state_id,
                s.delayed.len(),
                s.chunks.len()
            );
            s.info(msg);
        }
        _ => s.info(HELP),
    }
    Ok(())
}
