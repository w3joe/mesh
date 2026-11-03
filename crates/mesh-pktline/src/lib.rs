use bytes::{BufMut, Bytes, BytesMut};
use mesh_pack::empty_packfile;
use mesh_proto::{ObjectId, RefPointer};
use thiserror::Error;

pub const FLUSH_PKT: &[u8] = b"0000";
pub const DELIM_PKT: &[u8] = b"0001";
pub const RESPONSE_END_PKT: &[u8] = b"0002";

#[derive(Debug, Error)]
pub enum PktLineError {
    #[error("malformed pkt-line length header: {0}")]
    InvalidLengthHeader(String),
    #[error("unexpected EOF while reading pkt-line")]
    UnexpectedEof,
    #[error("malformed ref update command: {0}")]
    MalformedCommand(String),
}

/// Encodes a payload into Git's 4-byte hex length-prefixed pkt-line format.
pub fn encode_pkt_line(payload: &[u8]) -> Bytes {
    let total_len = payload.len() + 4;
    let header = format!("{total_len:04x}");
    let mut buf = BytesMut::with_capacity(total_len);
    buf.put_slice(header.as_bytes());
    buf.put_slice(payload);
    buf.freeze()
}

/// Encodes a sideband-64k packet (`band = 1` for pack data, `2` for progress, `3` for error).
pub fn encode_sideband_pkt(band: u8, payload: &[u8]) -> Bytes {
    let mut buf = BytesMut::new();
    // Max payload per pkt-line is 65515 bytes (65520 - 4 header - 1 band)
    for chunk in payload.chunks(65500) {
        let total_len = chunk.len() + 5;
        let header = format!("{total_len:04x}");
        buf.put_slice(header.as_bytes());
        buf.put_u8(band);
        buf.put_slice(chunk);
    }
    buf.freeze()
}

/// Builds the Git Smart HTTP `/info/refs?service=...` advertisement.
/// For `git-receive-pack`, callers pass only `trunk` (+ optional `target_ref`) so the payload
/// remains $O(1)$ (<256 bytes) regardless of how many million branches exist.
pub fn build_info_refs_advertisement(service: &str, refs: &[RefPointer]) -> Bytes {
    let mut out = BytesMut::new();
    let header_line = format!("# service={service}\n");
    out.put_slice(&encode_pkt_line(header_line.as_bytes()));
    out.put_slice(FLUSH_PKT);

    let capabilities = if service == "git-receive-pack" {
        "report-status side-band-64k quiet atomic ofs-delta agent=mesh/0.1.0"
    } else {
        "multi_ack_detailed no-done side-band-64k ofs-delta allow-tip-sha1-in-want allow-reachable-sha1-in-want packfile-uris agent=mesh/0.1.0"
    };

    if refs.is_empty() {
        if service == "git-receive-pack" {
            let line = format!(
                "{} capabilities^{{}}\0{}\n",
                ObjectId::ZERO.to_hex(),
                capabilities
            );
            out.put_slice(&encode_pkt_line(line.as_bytes()));
        }
    } else {
        // If `refs/heads/main` is present during upload-pack, also advertise `HEAD` pointing to it
        if service == "git-upload-pack" {
            if let Some(main_ref) = refs.iter().find(|r| r.ref_name == "refs/heads/main") {
                let head_line = format!(
                    "{} HEAD\0{} symref=HEAD:refs/heads/main\n",
                    main_ref.tip_commit_oid.to_hex(),
                    capabilities
                );
                out.put_slice(&encode_pkt_line(head_line.as_bytes()));
                for r in refs {
                    let line = format!("{} {}\n", r.tip_commit_oid.to_hex(), r.ref_name);
                    out.put_slice(&encode_pkt_line(line.as_bytes()));
                }
                out.put_slice(FLUSH_PKT);
                return out.freeze();
            }
        }

        for (idx, r) in refs.iter().enumerate() {
            let line = if idx == 0 {
                format!(
                    "{} {}\0{}\n",
                    r.tip_commit_oid.to_hex(),
                    r.ref_name,
                    capabilities
                )
            } else {
                format!("{} {}\n", r.tip_commit_oid.to_hex(), r.ref_name)
            };
            out.put_slice(&encode_pkt_line(line.as_bytes()));
        }
    }

    out.put_slice(FLUSH_PKT);
    out.freeze()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdateCommand {
    pub old_oid: ObjectId,
    pub new_oid: ObjectId,
    pub ref_name: String,
}

#[derive(Clone, Debug)]
pub struct ReceivePackRequest {
    pub commands: Vec<RefUpdateCommand>,
    pub capabilities: Vec<String>,
    pub pack_bytes: Bytes,
}

/// Parses a `POST /git-receive-pack` body into ref update commands and the raw packfile stream.
pub fn parse_receive_pack_request(body: &[u8]) -> Result<ReceivePackRequest, PktLineError> {
    let mut pos = 0usize;
    let mut commands = Vec::new();
    let mut capabilities = Vec::new();

    while pos + 4 <= body.len() {
        let len_hex = std::str::from_utf8(&body[pos..pos + 4])
            .map_err(|_| PktLineError::InvalidLengthHeader("non-utf8".to_string()))?;
        if len_hex == "0000" {
            pos += 4;
            break;
        }
        let pkt_len = usize::from_str_radix(len_hex, 16)
            .map_err(|_| PktLineError::InvalidLengthHeader(len_hex.to_string()))?;
        if pkt_len < 4 || pos + pkt_len > body.len() {
            return Err(PktLineError::UnexpectedEof);
        }
        let payload = &body[pos + 4..pos + pkt_len];
        pos += pkt_len;

        // First command may contain `\0<capabilities>`
        let (cmd_bytes, caps_bytes) = match payload.iter().position(|&b| b == 0) {
            Some(nul_idx) => (&payload[..nul_idx], Some(&payload[nul_idx + 1..])),
            None => (payload, None),
        };

        if let Some(caps) = caps_bytes {
            let caps_str = String::from_utf8_lossy(caps);
            for c in caps_str.split_whitespace() {
                capabilities.push(c.to_string());
            }
        }

        let cmd_str = String::from_utf8_lossy(cmd_bytes);
        let parts: Vec<&str> = cmd_str.trim().split_whitespace().collect();
        if parts.len() < 3 {
            return Err(PktLineError::MalformedCommand(cmd_str.to_string()));
        }
        let old_oid = ObjectId::from_hex(parts[0])
            .map_err(|e| PktLineError::MalformedCommand(e.to_string()))?;
        let new_oid = ObjectId::from_hex(parts[1])
            .map_err(|e| PktLineError::MalformedCommand(e.to_string()))?;
        let ref_name = parts[2].to_string();

        commands.push(RefUpdateCommand {
            old_oid,
            new_oid,
            ref_name,
        });
    }

    let pack_bytes = Bytes::copy_from_slice(&body[pos..]);
    Ok(ReceivePackRequest {
        commands,
        capabilities,
        pack_bytes,
    })
}

#[derive(Clone, Debug)]
pub struct RefUpdateResult {
    pub ref_name: String,
    pub ok: bool,
    pub message: Option<String>,
}

/// Encodes the `git-receive-pack` response (`unpack ok` + per-ref status + optional progress messages).
pub fn encode_receive_pack_report(
    results: &[RefUpdateResult],
    progress_messages: &[String],
    use_sideband: bool,
) -> Bytes {
    let mut status_buf = BytesMut::new();
    status_buf.put_slice(&encode_pkt_line(b"unpack ok\n"));
    for res in results {
        if res.ok {
            let line = format!("ok {}\n", res.ref_name);
            status_buf.put_slice(&encode_pkt_line(line.as_bytes()));
        } else {
            let reason = res.message.as_deref().unwrap_or("rejected");
            let line = format!("ng {} {}\n", res.ref_name, reason);
            status_buf.put_slice(&encode_pkt_line(line.as_bytes()));
        }
    }
    status_buf.put_slice(FLUSH_PKT);

    if !use_sideband {
        return status_buf.freeze();
    }

    let mut out = BytesMut::new();
    for msg in progress_messages {
        let formatted = if msg.ends_with('\n') {
            msg.clone()
        } else {
            format!("{msg}\n")
        };
        out.put_slice(&encode_sideband_pkt(2, formatted.as_bytes()));
    }
    out.put_slice(&encode_sideband_pkt(1, &status_buf));
    out.put_slice(FLUSH_PKT);
    out.freeze()
}

#[derive(Clone, Debug, Default)]
pub struct UploadPackRequest {
    pub wants: Vec<ObjectId>,
    pub haves: Vec<ObjectId>,
    pub supports_packfile_uris: bool,
    pub supports_sideband: bool,
    pub done: bool,
}

/// Parses a `POST /git-upload-pack` request (supporting both Smart HTTP v1 and Protocol v2).
pub fn parse_upload_pack_request(body: &[u8]) -> Result<UploadPackRequest, PktLineError> {
    let mut pos = 0usize;
    let mut req = UploadPackRequest::default();

    while pos + 4 <= body.len() {
        let len_hex = std::str::from_utf8(&body[pos..pos + 4])
            .map_err(|_| PktLineError::InvalidLengthHeader("non-utf8".to_string()))?;
        if len_hex == "0000" || len_hex == "0001" || len_hex == "0002" {
            pos += 4;
            continue;
        }
        let pkt_len = usize::from_str_radix(len_hex, 16)
            .map_err(|_| PktLineError::InvalidLengthHeader(len_hex.to_string()))?;
        if pkt_len < 4 || pos + pkt_len > body.len() {
            return Err(PktLineError::UnexpectedEof);
        }
        let payload = &body[pos + 4..pos + pkt_len];
        pos += pkt_len;

        let line = String::from_utf8_lossy(payload);
        let main_part = line.split('\0').next().unwrap_or("").trim();
        if line.contains("side-band-64k") || line.contains("side-band") {
            req.supports_sideband = true;
        }
        if line.contains("packfile-uris") {
            req.supports_packfile_uris = true;
        }

        if let Some(rest) = main_part.strip_prefix("want ") {
            let oid_str = rest.split_whitespace().next().unwrap_or("");
            if let Ok(oid) = ObjectId::from_hex(oid_str) {
                if !req.wants.contains(&oid) {
                    req.wants.push(oid);
                }
            }
        } else if let Some(rest) = main_part.strip_prefix("have ") {
            let oid_str = rest.split_whitespace().next().unwrap_or("");
            if let Ok(oid) = ObjectId::from_hex(oid_str) {
                if !req.haves.contains(&oid) {
                    req.haves.push(oid);
                }
            }
        } else if main_part == "done" {
            req.done = true;
        }
    }

    Ok(req)
}

/// Encodes a Pando `packfile-uri` response:
/// Sends the list of Express CDN URLs in the `packfile-uris` section and a 32-byte empty packfile inline.
pub fn encode_packfile_uri_response(pack_uris: &[String]) -> Bytes {
    let mut out = BytesMut::new();
    out.put_slice(&encode_pkt_line(b"packfile-uris\n"));
    for uri in pack_uris {
        let line = format!("{uri}\n");
        out.put_slice(&encode_pkt_line(line.as_bytes()));
    }
    out.put_slice(DELIM_PKT);
    out.put_slice(&encode_pkt_line(b"packfile\n"));
    let empty = empty_packfile();
    out.put_slice(&encode_sideband_pkt(1, &empty));
    out.put_slice(FLUSH_PKT);
    out.freeze()
}

/// Encodes a standard Smart HTTP v1 `git-upload-pack` response for stock Git clients.
pub fn encode_standard_upload_pack_response(
    pack_bytes: &[u8],
    common_have: Option<ObjectId>,
    progress_info: Option<&str>,
    use_sideband: bool,
) -> Bytes {
    let mut out = BytesMut::new();
    if let Some(oid) = common_have {
        let ack = format!("ACK {} common\n", oid.to_hex());
        out.put_slice(&encode_pkt_line(ack.as_bytes()));
        let ready = format!("ACK {}\n", oid.to_hex());
        out.put_slice(&encode_pkt_line(ready.as_bytes()));
    } else {
        out.put_slice(&encode_pkt_line(b"NAK\n"));
    }

    if use_sideband {
        if let Some(info) = progress_info {
            let msg = format!("{info}\n");
            out.put_slice(&encode_sideband_pkt(2, msg.as_bytes()));
        }
        out.put_slice(&encode_sideband_pkt(1, pack_bytes));
        out.put_slice(FLUSH_PKT);
    } else {
        out.put_slice(pack_bytes);
    }
    out.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trunk_only_advertisement_stays_small() {
        let ptr = RefPointer {
            repo_id: "openai/monorepo".to_string(),
            ref_name: "refs/heads/main".to_string(),
            current_version: 42,
            tip_commit_oid: ObjectId::from_hex("da39a3ee5e6b4b0d3255bfef95601890afd80709")
                .unwrap(),
            base_trunk_version: None,
        };
        let adv = build_info_refs_advertisement("git-receive-pack", &[ptr]);
        assert!(adv.len() < 256, "advertisement was {} bytes", adv.len());
    }
}
