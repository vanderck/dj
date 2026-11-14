//! OSC over UDP: a listener thread and a feedback sender.

use std::net::UdpSocket;

use crossbeam_channel::{unbounded, Receiver};
use rosc::{OscMessage, OscPacket, OscType};

fn flatten(p: OscPacket, out: &mut Vec<(String, Vec<OscType>)>) {
    match p {
        OscPacket::Message(m) => {
            if m.addr.len() < 256 {
                out.push((m.addr, m.args));
            }
        }
        // Bundles are applied in order as they arrive; time tags are ignored.
        OscPacket::Bundle(b) => {
            for c in b.content {
                flatten(c, out);
            }
        }
    }
}

pub fn listen(port: u16) -> std::io::Result<Receiver<(String, Vec<OscType>)>> {
    let sock = UdpSocket::bind(("0.0.0.0", port))?;
    let (tx, rx) = unbounded();
    std::thread::Builder::new().name("osc-in".into()).spawn(move || {
        let mut buf = vec![0u8; 65536];
        loop {
            let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
            if let Ok((_, packet)) = rosc::decoder::decode_udp(&buf[..n]) {
                let mut msgs = Vec::new();
                flatten(packet, &mut msgs);
                for m in msgs {
                    if tx.send(m).is_err() {
                        return;
                    }
                }
            }
        }
    })?;
    Ok(rx)
}

pub struct Sender {
    sock: UdpSocket,
    dest: String,
}

impl Sender {
    pub fn new(host: &str, port: u16) -> std::io::Result<Self> {
        let sock = UdpSocket::bind(("0.0.0.0", 0))?;
        Ok(Self { sock, dest: format!("{host}:{port}") })
    }

    pub fn send(&self, addr: &str, args: Vec<OscType>) {
        let p = OscPacket::Message(OscMessage { addr: addr.to_string(), args });
        if let Ok(buf) = rosc::encoder::encode(&p) {
            let _ = self.sock.send_to(&buf, &self.dest);
        }
    }
}
