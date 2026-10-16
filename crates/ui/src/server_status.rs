// SPDX-License-Identifier: AGPL-3.0-only

//! Minecraft Java Server List Ping (status + ping round-trip).
//!
//! Protocol (wiki.vg "Server List Ping"):
//! 1. Handshake packet (id `0x00`): protocol version (VarInt) + server
//!    address (VarInt-prefixed UTF-8) + port (`u16` BE) + next state
//!    VarInt `1` (status).
//! 2. Status Request packet (id `0x00`, empty).
//! 3. Server replies with Status Response (id `0x00` + VarInt-prefixed JSON:
//!    `version.name`, `players.online/max`, `description`, `favicon`).
//! 4. Client sends Ping Request (id `0x01` + `i64` BE payload); the server
//!    echoes it back (Pong). The round-trip time is the displayed ping.
//!
//! TPS is NOT part of this protocol — callers must surface `tps: None`.

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Ping/connect timeout (whole status + ping exchange is bounded by this).
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// Protocol version sent in the handshake (1.7.10). The server ignores it
/// for status purposes; any valid VarInt works.
pub const HANDSHAKE_PROTOCOL_VERSION: i32 = 5;
/// Next-state `1` = status.
pub const NEXT_STATE_STATUS: i32 = 1;

/// Parsed fields extracted from the status JSON payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusInfo {
    /// `version.name` (e.g. `"1.7.10"`).
    pub version: String,
    /// `players.online`.
    pub players_online: u32,
    /// `players.max`.
    pub players_max: u32,
    /// Plain-text MOTD (§ codes stripped).
    pub motd: String,
}

/// Encode a signed VarInt (up to 5 bytes, little-endian 7-bit groups).
pub fn encode_varint(value: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    let mut v = value as u32;
    loop {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
    out
}

/// Decode a VarInt from the front of `buf`. Returns `(value, bytes_read)`.
pub fn decode_varint(buf: &[u8]) -> Result<(i32, usize), String> {
    let mut value: u32 = 0;
    let mut shift = 0u32;
    for (i, &b) in buf.iter().enumerate() {
        if i >= 5 {
            return Err("VarInt trop long.".to_string());
        }
        value |= ((b & 0x7F) as u32) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok((value as i32, i + 1));
        }
    }
    Err("VarInt tronqué.".to_string())
}

/// Build the handshake packet (length-prefixed) for `host:port`.
pub fn build_handshake_packet(host: &str, port: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(0x00);
    body.extend(encode_varint(HANDSHAKE_PROTOCOL_VERSION));
    let host_bytes = host.as_bytes();
    body.extend(encode_varint(host_bytes.len() as i32));
    body.extend_from_slice(host_bytes);
    body.extend_from_slice(&port.to_be_bytes());
    body.extend(encode_varint(NEXT_STATE_STATUS));
    let mut out = encode_varint(body.len() as i32);
    out.extend(body);
    out
}

/// Build the empty status-request packet (length `1`, id `0x00`).
pub fn build_status_request_packet() -> Vec<u8> {
    vec![0x01, 0x00]
}

/// Build the ping-request packet (id `0x01` + `i64` BE payload).
pub fn build_ping_request_packet(payload: i64) -> Vec<u8> {
    let mut out = vec![0x09, 0x01];
    out.extend_from_slice(&payload.to_be_bytes());
    out
}

/// Strip Minecraft `§` formatting codes (`§` + following char removed).
pub fn strip_minecraft_codes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            let _ = chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

/// Flatten a `description` value (string | `{text,extra}` object | array)
/// into plain text, then strip `§` codes.
pub fn description_to_plain(description: &serde_json::Value) -> String {
    fn walk(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::String(s) => out.push_str(s),
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(t)) = map.get("text") {
                    out.push_str(t);
                }
                if let Some(extra) = map.get("extra") {
                    walk(extra, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            _ => {}
        }
    }
    let mut raw = String::new();
    walk(description, &mut raw);
    strip_minecraft_codes(&raw).trim().to_string()
}

/// Parse the status-response JSON (`favicon` ignored).
pub fn parse_status_json(json: &str) -> Result<StatusInfo, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("Réponse statut illisible : {e}"))?;
    let version = v
        .get("version")
        .and_then(|x| x.get("name"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let players_online = v
        .get("players")
        .and_then(|x| x.get("online"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    let players_max = v
        .get("players")
        .and_then(|x| x.get("max"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    let motd = match v.get("description") {
        Some(d) => description_to_plain(d),
        None => String::new(),
    };
    Ok(StatusInfo {
        version,
        players_online,
        players_max,
        motd,
    })
}

async fn read_varint<R>(r: &mut R) -> Result<i32, String>
where
    R: AsyncReadExt + Unpin,
{
    let mut value: u32 = 0;
    let mut shift = 0u32;
    for _ in 0..5 {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)
            .await
            .map_err(|e| format!("Lecture statut : {e}"))?;
        value |= ((b[0] & 0x7F) as u32) << shift;
        if b[0] & 0x80 == 0 {
            return Ok(value as i32);
        }
        shift += 7;
    }
    Err("VarInt trop long.".to_string())
}

async fn read_exact_vec<R>(r: &mut R, len: usize) -> Result<Vec<u8>, String>
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)
        .await
        .map_err(|e| format!("Lecture statut : {e}"))?;
    Ok(buf)
}

/// Full status + ping exchange against `host:port` (bounded by `timeout`).
pub async fn ping_server(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<(StatusInfo, u64), String> {
    let addr = format!("{host}:{port}");
    let mut stream = tokio::time::timeout(timeout, TcpStream::connect(addr.as_str()))
        .await
        .map_err(|_| format!("Serveur injoignable ({addr}) : délai dépassé."))?
        .map_err(|e| format!("Serveur injoignable ({addr}) : {e}"))?;

    let handshake = build_handshake_packet(host, port);
    let status_req = build_status_request_packet();
    tokio::time::timeout(timeout, async {
        stream
            .write_all(&handshake)
            .await
            .map_err(|e| format!("Écriture statut : {e}"))?;
        stream
            .write_all(&status_req)
            .await
            .map_err(|e| format!("Écriture statut : {e}"))?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|_| format!("Serveur injoignable ({addr}) : délai dépassé."))??;

    // Status Response: length + packet id (0x00) + JSON string.
    let json = tokio::time::timeout(timeout, async {
        let _len = read_varint(&mut stream).await?;
        let packet_id = read_varint(&mut stream).await?;
        if packet_id != 0x00 {
            return Err(format!("Réponse statut inattendue (id {packet_id})."));
        }
        let json_len = read_varint(&mut stream).await?;
        if json_len < 0 || json_len as usize > 1_000_000 {
            return Err("Réponse statut trop volumineuse.".to_string());
        }
        let bytes = read_exact_vec(&mut stream, json_len as usize).await?;
        String::from_utf8(bytes).map_err(|e| format!("Réponse statut illisible : {e}"))
    })
    .await
    .map_err(|_| format!("Serveur injoignable ({addr}) : délai dépassé."))??;
    let info = parse_status_json(&json)?;

    // Ping/Pong round-trip for the displayed latency.
    let payload = chrono_millis();
    let ping_pkt = build_ping_request_packet(payload);
    let start = Instant::now();
    tokio::time::timeout(timeout, async {
        stream
            .write_all(&ping_pkt)
            .await
            .map_err(|e| format!("Écriture ping : {e}"))?;
        let _len = read_varint(&mut stream).await?;
        let packet_id = read_varint(&mut stream).await?;
        if packet_id != 0x01 {
            return Err(format!("Réponse ping inattendue (id {packet_id})."));
        }
        let bytes = read_exact_vec(&mut stream, 8).await?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(&bytes);
        let echoed = i64::from_be_bytes(arr);
        if echoed != payload {
            return Err("Réponse ping incohérente.".to_string());
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|_| format!("Serveur injoignable ({addr}) : délai dépassé."))??;
    Ok((info, start.elapsed().as_millis() as u64))
}

fn chrono_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrips_edge_values() {
        for v in [0, 1, 2, 127, 128, 255, 2147483647, -1, -2147483648] {
            let enc = encode_varint(v);
            let (dec, n) = decode_varint(&enc).unwrap();
            assert_eq!(dec, v, "value {v}");
            assert_eq!(n, enc.len());
        }
        assert_eq!(encode_varint(0), vec![0x00]);
        assert_eq!(encode_varint(1), vec![0x01]);
        assert_eq!(encode_varint(128), vec![0x80, 0x01]);
    }

    #[test]
    fn handshake_packet_shape() {
        let pkt = build_handshake_packet("mc.46anarchy.fr", 25566);
        let (len, n) = decode_varint(&pkt).unwrap();
        assert_eq!(len as usize, pkt.len() - n);
        let body = &pkt[n..];
        assert_eq!(body[0], 0x00);
        let (proto, _) = decode_varint(&body[1..]).unwrap();
        assert_eq!(proto, HANDSHAKE_PROTOCOL_VERSION);
        // Host bytes + big-endian port must be embedded verbatim.
        assert!(body
            .windows(b"mc.46anarchy.fr".len())
            .any(|w| w == b"mc.46anarchy.fr"));
        assert!(body.windows(2).any(|w| w == 25566u16.to_be_bytes()));
        assert_eq!(*body.last().unwrap(), NEXT_STATE_STATUS as u8);
    }

    #[test]
    fn status_and_ping_packets_shape() {
        assert_eq!(build_status_request_packet(), vec![0x01, 0x00]);
        let payload = 123456789i64;
        let pkt = build_ping_request_packet(payload);
        assert_eq!(&pkt[..2], &[0x09, 0x01]);
        assert_eq!(&pkt[2..], &payload.to_be_bytes());
    }

    #[test]
    fn parses_vanilla_status_fixture() {
        let json = r#"{
            "version": {"name": "1.7.10", "protocol": 5},
            "players": {"max": 100, "online": 7, "sample": []},
            "description": {"text": "§a46Anarchy §7- §fServeur PvP"},
            "favicon": "data:image/png;base64,ignored"
        }"#;
        let info = parse_status_json(json).unwrap();
        assert_eq!(info.version, "1.7.10");
        assert_eq!(info.players_online, 7);
        assert_eq!(info.players_max, 100);
        assert_eq!(info.motd, "46Anarchy - Serveur PvP");
    }

    #[test]
    fn parses_legacy_string_motd_and_strips_codes() {
        let json = r#"{
            "version": {"name": "Forge 1.7.10"},
            "players": {"max": 50, "online": 0},
            "description": "§bHello §lWorld§r!"
        }"#;
        let info = parse_status_json(json).unwrap();
        assert_eq!(info.motd, "Hello World!");
        assert_eq!(strip_minecraft_codes("§aX§bY§"), "XY");
    }

    #[test]
    fn parses_extra_parts_motd() {
        let json = r#"{
            "version": {"name": "1.7.10"},
            "players": {"max": 10, "online": 1},
            "description": {"text": "", "extra": [{"text": "§cA"}, {"text": "B"}]}
        }"#;
        assert_eq!(parse_status_json(json).unwrap().motd, "AB");
    }
}
