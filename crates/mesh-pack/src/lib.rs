use bytes::{BufMut, Bytes, BytesMut};
use flate2::write::ZlibEncoder;
use flate2::{Compression, Decompress, FlushDecompress};
use mesh_proto::ObjectId;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PackError {
    #[error("packfile too short ({0} bytes)")]
    TooShort(usize),
    #[error("invalid packfile magic header")]
    InvalidMagic,
    #[error("unsupported packfile version {0}")]
    UnsupportedVersion(u32),
    #[error("packfile SHA-1 trailer mismatch: expected {expected}, computed {computed}")]
    ChecksumMismatch {
        expected: ObjectId,
        computed: ObjectId,
    },
    #[error("zlib decompression error: {0}")]
    Zlib(String),
    #[error("delta base not found at offset {0}")]
    MissingOffsetBase(usize),
    #[error("delta base object {0} not found in pack or trunk index")]
    MissingRefBase(ObjectId),
    #[error("malformed delta instruction stream")]
    MalformedDelta,
    #[error("invalid git object type {0}")]
    InvalidObjectType(u8),
    #[error("malformed commit or tree object: {0}")]
    MalformedObject(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitObjectType {
    Commit = 1,
    Tree = 2,
    Blob = 3,
    Tag = 4,
}

impl GitObjectType {
    pub fn as_str(&self) -> &'static str {
        match self {
            GitObjectType::Commit => "commit",
            GitObjectType::Tree => "tree",
            GitObjectType::Blob => "blob",
            GitObjectType::Tag => "tag",
        }
    }

    pub fn from_u8(v: u8) -> Result<Self, PackError> {
        match v {
            1 => Ok(GitObjectType::Commit),
            2 => Ok(GitObjectType::Tree),
            3 => Ok(GitObjectType::Blob),
            4 => Ok(GitObjectType::Tag),
            other => Err(PackError::InvalidObjectType(other)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitObject {
    pub oid: ObjectId,
    pub obj_type: GitObjectType,
    pub data: Bytes,
}

impl GitObject {
    pub fn new(obj_type: GitObjectType, data: impl Into<Bytes>) -> Self {
        let data = data.into();
        let oid = compute_git_oid(obj_type, &data);
        Self {
            oid,
            obj_type,
            data,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackIndexEntry {
    pub oid: ObjectId,
    pub obj_type: GitObjectType,
    pub offset: u64,
    pub uncompressed_size: usize,
    pub crc32: u32,
}

#[derive(Clone, Debug)]
pub struct DecodedPack {
    pub objects: Vec<GitObject>,
    pub index_entries: Vec<PackIndexEntry>,
    pub pack_checksum: ObjectId,
}

/// Computes the canonical 20-byte Git SHA-1 ObjectId (`"{type} {len}\0{data}"`).
pub fn compute_git_oid(obj_type: GitObjectType, data: &[u8]) -> ObjectId {
    let mut hasher = Sha1::new();
    let header = format!("{} {}\0", obj_type.as_str(), data.len());
    hasher.update(header.as_bytes());
    hasher.update(data);
    let digest = hasher.finalize();
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&digest);
    ObjectId::from_bytes(arr)
}

/// Returns the canonical 32-byte empty Git v2 packfile used when `packfile-uri` offloads all packs.
pub fn empty_packfile() -> Bytes {
    encode_packfile(&[])
}

/// Encodes a slice of `GitObject`s into a valid Git Packfile v2 (`PACK` + objects + SHA-1 trailer).
pub fn encode_packfile(objects: &[GitObject]) -> Bytes {
    let mut buf = BytesMut::new();
    buf.put_slice(b"PACK");
    buf.put_u32(2); // Version 2
    buf.put_u32(objects.len() as u32);

    for obj in objects {
        let type_num = obj.obj_type as u8;
        let mut size = obj.data.len();
        let mut first = (type_num << 4) | ((size & 0x0f) as u8);
        size >>= 4;
        if size > 0 {
            first |= 0x80;
        }
        buf.put_u8(first);
        while size > 0 {
            let mut byte = (size & 0x7f) as u8;
            size >>= 7;
            if size > 0 {
                byte |= 0x80;
            }
            buf.put_u8(byte);
        }

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&obj.data).expect("zlib compress");
        let compressed = encoder.finish().expect("zlib finish");
        buf.put_slice(&compressed);
    }

    let mut sha1 = Sha1::new();
    sha1.update(&buf);
    let digest = sha1.finalize();
    buf.put_slice(&digest);
    buf.freeze()
}

/// Single-pass streaming validator & decoder for Git Packfile v2.
/// Never writes loose objects to disk (`git unpack-objects` is never invoked).
/// Uses an $O(1)$ point-lookup callback `external_base_lookup` for `REF_DELTA` bases.
pub fn decode_and_validate_pack<F>(
    pack_bytes: &[u8],
    external_base_lookup: F,
) -> Result<DecodedPack, PackError>
where
    F: Fn(&ObjectId) -> Option<GitObject>,
{
    if pack_bytes.len() < 32 {
        return Err(PackError::TooShort(pack_bytes.len()));
    }
    if &pack_bytes[0..4] != b"PACK" {
        return Err(PackError::InvalidMagic);
    }
    let version = u32::from_be_bytes([pack_bytes[4], pack_bytes[5], pack_bytes[6], pack_bytes[7]]);
    if version != 2 {
        return Err(PackError::UnsupportedVersion(version));
    }
    let obj_count =
        u32::from_be_bytes([pack_bytes[8], pack_bytes[9], pack_bytes[10], pack_bytes[11]]) as usize;

    let body_end = pack_bytes.len() - 20;
    let expected_checksum = ObjectId::from_slice(&pack_bytes[body_end..]).unwrap();
    let mut sha1 = Sha1::new();
    sha1.update(&pack_bytes[..body_end]);
    let computed_checksum = ObjectId::from_slice(&sha1.finalize()).unwrap();

    if expected_checksum != computed_checksum {
        return Err(PackError::ChecksumMismatch {
            expected: expected_checksum,
            computed: computed_checksum,
        });
    }

    let mut cursor = 12usize;
    let mut objects = Vec::with_capacity(obj_count);
    let mut index_entries = Vec::with_capacity(obj_count);
    let mut by_offset: HashMap<usize, GitObject> = HashMap::with_capacity(obj_count);
    let mut by_oid: HashMap<ObjectId, GitObject> = HashMap::with_capacity(obj_count);

    for _ in 0..obj_count {
        let entry_start = cursor;
        if cursor >= body_end {
            return Err(PackError::TooShort(pack_bytes.len()));
        }
        let b0 = pack_bytes[cursor];
        cursor += 1;
        let raw_type = (b0 >> 4) & 0x07;
        let mut size = (b0 & 0x0f) as usize;
        let mut shift = 4usize;
        let mut prev = b0;
        while (prev & 0x80) != 0 {
            if cursor >= body_end {
                return Err(PackError::TooShort(pack_bytes.len()));
            }
            let b = pack_bytes[cursor];
            cursor += 1;
            size |= ((b & 0x7f) as usize) << shift;
            shift += 7;
            prev = b;
        }

        let resolved_obj = match raw_type {
            1..=4 => {
                let obj_type = GitObjectType::from_u8(raw_type)?;
                let (inflated, consumed) = inflate_zlib_entry(&pack_bytes[cursor..body_end], size)?;
                cursor += consumed;
                GitObject::new(obj_type, Bytes::from(inflated))
            }
            6 => {
                // OFS_DELTA: negative byte offset from `entry_start`
                let mut b = pack_bytes[cursor];
                cursor += 1;
                let mut neg_offset = (b & 0x7f) as usize;
                while (b & 0x80) != 0 {
                    b = pack_bytes[cursor];
                    cursor += 1;
                    neg_offset = ((neg_offset + 1) << 7) | ((b & 0x7f) as usize);
                }
                let base_offset = entry_start
                    .checked_sub(neg_offset)
                    .ok_or(PackError::MissingOffsetBase(neg_offset))?;
                let base_obj = by_offset
                    .get(&base_offset)
                    .ok_or(PackError::MissingOffsetBase(base_offset))?
                    .clone();
                let (delta_bytes, consumed) =
                    inflate_zlib_entry(&pack_bytes[cursor..body_end], size)?;
                cursor += consumed;
                let reconstructed = apply_git_delta(&base_obj.data, &delta_bytes)?;
                GitObject::new(base_obj.obj_type, Bytes::from(reconstructed))
            }
            7 => {
                // REF_DELTA: 20-byte base ObjectId
                if cursor + 20 > body_end {
                    return Err(PackError::TooShort(pack_bytes.len()));
                }
                let base_oid = ObjectId::from_slice(&pack_bytes[cursor..cursor + 20]).unwrap();
                cursor += 20;
                let base_obj = by_oid
                    .get(&base_oid)
                    .cloned()
                    .or_else(|| external_base_lookup(&base_oid))
                    .ok_or(PackError::MissingRefBase(base_oid))?;
                let (delta_bytes, consumed) =
                    inflate_zlib_entry(&pack_bytes[cursor..body_end], size)?;
                cursor += consumed;
                let reconstructed = apply_git_delta(&base_obj.data, &delta_bytes)?;
                GitObject::new(base_obj.obj_type, Bytes::from(reconstructed))
            }
            other => return Err(PackError::InvalidObjectType(other)),
        };

        let entry_end = cursor;
        let crc32 = crc32fast::hash(&pack_bytes[entry_start..entry_end]);
        index_entries.push(PackIndexEntry {
            oid: resolved_obj.oid,
            obj_type: resolved_obj.obj_type,
            offset: entry_start as u64,
            uncompressed_size: resolved_obj.data.len(),
            crc32,
        });
        by_offset.insert(entry_start, resolved_obj.clone());
        by_oid.insert(resolved_obj.oid, resolved_obj.clone());
        objects.push(resolved_obj);
    }

    Ok(DecodedPack {
        objects,
        index_entries,
        pack_checksum: expected_checksum,
    })
}

fn inflate_zlib_entry(input: &[u8], expected_size: usize) -> Result<(Vec<u8>, usize), PackError> {
    let mut decompressor = Decompress::new(true);
    let mut out = Vec::with_capacity(expected_size.max(64));
    let mut in_pos = 0usize;
    let mut scratch = [0u8; 8192];

    loop {
        let before_in = decompressor.total_in() as usize;
        let before_out = decompressor.total_out() as usize;
        let status = decompressor
            .decompress(&input[in_pos..], &mut scratch, FlushDecompress::None)
            .map_err(|e| PackError::Zlib(e.to_string()))?;
        let consumed = (decompressor.total_in() as usize) - before_in;
        let produced = (decompressor.total_out() as usize) - before_out;
        in_pos += consumed;
        if produced > 0 {
            out.extend_from_slice(&scratch[..produced]);
        }
        match status {
            flate2::Status::StreamEnd => break,
            flate2::Status::Ok | flate2::Status::BufError => {
                if consumed == 0 && produced == 0 {
                    return Err(PackError::Zlib("premature EOF in zlib stream".to_string()));
                }
            }
        }
    }

    Ok((out, in_pos))
}

/// Applies a Git packfile delta stream (`OBJ_OFS_DELTA` / `OBJ_REF_DELTA`) to `base`.
pub fn apply_git_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, PackError> {
    let mut pos = 0usize;
    let (src_size, c1) = read_delta_varint(&delta[pos..])?;
    pos += c1;
    if src_size != base.len() {
        return Err(PackError::MalformedDelta);
    }
    let (dst_size, c2) = read_delta_varint(&delta[pos..])?;
    pos += c2;

    let mut out = Vec::with_capacity(dst_size);
    while pos < delta.len() {
        let cmd = delta[pos];
        pos += 1;
        if (cmd & 0x80) != 0 {
            // Copy from base object
            let mut cp_off = 0usize;
            let mut cp_size = 0usize;
            if (cmd & 0x01) != 0 {
                cp_off |= delta[pos] as usize;
                pos += 1;
            }
            if (cmd & 0x02) != 0 {
                cp_off |= (delta[pos] as usize) << 8;
                pos += 1;
            }
            if (cmd & 0x04) != 0 {
                cp_off |= (delta[pos] as usize) << 16;
                pos += 1;
            }
            if (cmd & 0x08) != 0 {
                cp_off |= (delta[pos] as usize) << 24;
                pos += 1;
            }
            if (cmd & 0x10) != 0 {
                cp_size |= delta[pos] as usize;
                pos += 1;
            }
            if (cmd & 0x20) != 0 {
                cp_size |= (delta[pos] as usize) << 8;
                pos += 1;
            }
            if (cmd & 0x40) != 0 {
                cp_size |= (delta[pos] as usize) << 16;
                pos += 1;
            }
            if cp_size == 0 {
                cp_size = 0x10000;
            }
            if cp_off + cp_size > base.len() {
                return Err(PackError::MalformedDelta);
            }
            out.extend_from_slice(&base[cp_off..cp_off + cp_size]);
        } else if cmd != 0 {
            // Insert literal bytes from delta
            let len = cmd as usize;
            if pos + len > delta.len() {
                return Err(PackError::MalformedDelta);
            }
            out.extend_from_slice(&delta[pos..pos + len]);
            pos += len;
        } else {
            return Err(PackError::MalformedDelta);
        }
    }

    if out.len() != dst_size {
        return Err(PackError::MalformedDelta);
    }
    Ok(out)
}

fn read_delta_varint(slice: &[u8]) -> Result<(usize, usize), PackError> {
    let mut val = 0usize;
    let mut shift = 0usize;
    let mut idx = 0usize;
    loop {
        if idx >= slice.len() {
            return Err(PackError::MalformedDelta);
        }
        let b = slice[idx];
        idx += 1;
        val |= ((b & 0x7f) as usize) << shift;
        if (b & 0x80) == 0 {
            break;
        }
        shift += 7;
    }
    Ok((val, idx))
}

/// Consolidates multiple `L(k)` packfiles into a single compacted `L(k+1)` packfile cover,
/// deduplicating shared objects while preserving order.
pub fn compact_packs<F>(
    packs: &[Bytes],
    external_base_lookup: F,
) -> Result<(Bytes, Vec<GitObject>), PackError>
where
    F: Fn(&ObjectId) -> Option<GitObject>,
{
    let mut combined_objects = Vec::new();
    let mut seen_oids: HashMap<ObjectId, GitObject> = HashMap::new();

    for pack in packs {
        let decoded = decode_and_validate_pack(pack, |oid| {
            seen_oids
                .get(oid)
                .cloned()
                .or_else(|| external_base_lookup(oid))
        })?;
        for obj in decoded.objects {
            if !seen_oids.contains_key(&obj.oid) {
                seen_oids.insert(obj.oid, obj.clone());
                combined_objects.push(obj);
            }
        }
    }

    let compacted_bytes = encode_packfile(&combined_objects);
    Ok((compacted_bytes, combined_objects))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCommit {
    pub tree_oid: ObjectId,
    pub parent_oids: Vec<ObjectId>,
    pub author: String,
    pub committer: String,
    pub message: String,
}

pub fn parse_commit(data: &[u8]) -> Result<ParsedCommit, PackError> {
    let text = std::str::from_utf8(data)
        .map_err(|e| PackError::MalformedObject(format!("commit not utf8: {e}")))?;
    let (headers, message) = text.split_once("\n\n").unwrap_or((text, ""));

    let mut tree_oid = None;
    let mut parent_oids = Vec::new();
    let mut author = String::new();
    let mut committer = String::new();

    for line in headers.lines() {
        if let Some(rest) = line.strip_prefix("tree ") {
            tree_oid = Some(
                ObjectId::from_hex(rest.trim())
                    .map_err(|e| PackError::MalformedObject(e.to_string()))?,
            );
        } else if let Some(rest) = line.strip_prefix("parent ") {
            parent_oids.push(
                ObjectId::from_hex(rest.trim())
                    .map_err(|e| PackError::MalformedObject(e.to_string()))?,
            );
        } else if let Some(rest) = line.strip_prefix("author ") {
            author = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("committer ") {
            committer = rest.to_string();
        }
    }

    let tree_oid =
        tree_oid.ok_or_else(|| PackError::MalformedObject("missing tree header".to_string()))?;
    Ok(ParsedCommit {
        tree_oid,
        parent_oids,
        author,
        committer,
        message: message.to_string(),
    })
}

pub fn build_commit_object(
    tree_oid: ObjectId,
    parents: &[ObjectId],
    author_name_email: &str,
    timestamp_sec: u64,
    message: &str,
) -> GitObject {
    let mut s = String::new();
    s.push_str(&format!("tree {}\n", tree_oid.to_hex()));
    for p in parents {
        if !p.is_zero() {
            s.push_str(&format!("parent {}\n", p.to_hex()));
        }
    }
    s.push_str(&format!("author {author_name_email} {timestamp_sec} +0000\n"));
    s.push_str(&format!(
        "committer {author_name_email} {timestamp_sec} +0000\n\n"
    ));
    s.push_str(message);
    if !message.ends_with('\n') {
        s.push('\n');
    }
    GitObject::new(GitObjectType::Commit, Bytes::from(s))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub name: String,
    pub oid: ObjectId,
}

pub fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry>, PackError> {
    let mut entries = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let space_rel = data[pos..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| PackError::MalformedObject("tree entry missing space".to_string()))?;
        let mode = std::str::from_utf8(&data[pos..pos + space_rel])
            .map_err(|e| PackError::MalformedObject(e.to_string()))?
            .to_string();
        pos += space_rel + 1;

        let nul_rel = data[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| PackError::MalformedObject("tree entry missing nul".to_string()))?;
        let name = std::str::from_utf8(&data[pos..pos + nul_rel])
            .map_err(|e| PackError::MalformedObject(e.to_string()))?
            .to_string();
        pos += nul_rel + 1;

        if pos + 20 > data.len() {
            return Err(PackError::MalformedObject(
                "tree entry truncated oid".to_string(),
            ));
        }
        let oid = ObjectId::from_slice(&data[pos..pos + 20]).unwrap();
        pos += 20;

        entries.push(TreeEntry { mode, name, oid });
    }
    Ok(entries)
}

pub fn build_tree_object(entries: &[TreeEntry]) -> GitObject {
    let mut sorted = entries.to_vec();
    // Git sorts tree entries by name, treating directories (mode "40000") as having a trailing '/'
    sorted.sort_by(|a, b| {
        let a_key = if a.mode == "40000" {
            format!("{}/", a.name)
        } else {
            a.name.clone()
        };
        let b_key = if b.mode == "40000" {
            format!("{}/", b.name)
        } else {
            b.name.clone()
        };
        a_key.cmp(&b_key)
    });

    let mut buf = BytesMut::new();
    for entry in sorted {
        buf.put_slice(entry.mode.as_bytes());
        buf.put_u8(b' ');
        buf.put_slice(entry.name.as_bytes());
        buf.put_u8(0);
        buf.put_slice(entry.oid.as_bytes());
    }
    GitObject::new(GitObjectType::Tree, buf.freeze())
}

/// Recursively walks a commit's root tree and returns a flat map of `file_path -> blob_oid`.
pub fn extract_commit_files<F>(
    commit_oid: &ObjectId,
    lookup: &F,
) -> Result<BTreeMap<String, ObjectId>, PackError>
where
    F: Fn(&ObjectId) -> Option<GitObject>,
{
    let Some(commit_obj) = lookup(commit_oid) else {
        return Ok(BTreeMap::new());
    };
    let parsed = parse_commit(&commit_obj.data)?;
    let mut out = BTreeMap::new();
    walk_tree_recursive(&parsed.tree_oid, "", lookup, &mut out)?;
    Ok(out)
}

fn walk_tree_recursive<F>(
    tree_oid: &ObjectId,
    prefix: &str,
    lookup: &F,
    out: &mut BTreeMap<String, ObjectId>,
) -> Result<(), PackError>
where
    F: Fn(&ObjectId) -> Option<GitObject>,
{
    let Some(tree_obj) = lookup(tree_oid) else {
        return Ok(());
    };
    let entries = parse_tree(&tree_obj.data)?;
    for entry in entries {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        if entry.mode == "40000" || entry.mode == "040000" {
            walk_tree_recursive(&entry.oid, &path, lookup, out)?;
        } else {
            out.insert(path, entry.oid);
        }
    }
    Ok(())
}

/// Builds hierarchical Git `tree` objects from a flat `file_path -> blob_oid` map.
/// Returns `(root_tree_oid, all_created_tree_objects)`.
pub fn build_trees_from_file_map(
    files: &BTreeMap<String, ObjectId>,
) -> (ObjectId, Vec<GitObject>) {
    enum Node {
        File(ObjectId),
        Dir(BTreeMap<String, Node>),
    }

    fn insert_path(dir: &mut BTreeMap<String, Node>, parts: &[&str], oid: ObjectId) {
        if parts.len() == 1 {
            dir.insert(parts[0].to_string(), Node::File(oid));
        } else {
            let sub = dir
                .entry(parts[0].to_string())
                .or_insert_with(|| Node::Dir(BTreeMap::new()));
            if let Node::Dir(sub_map) = sub {
                insert_path(sub_map, &parts[1..], oid);
            }
        }
    }

    fn materialize_dir(
        dir: &BTreeMap<String, Node>,
        out_trees: &mut Vec<GitObject>,
    ) -> ObjectId {
        let mut entries = Vec::new();
        for (name, node) in dir {
            match node {
                Node::File(oid) => entries.push(TreeEntry {
                    mode: "100644".to_string(),
                    name: name.clone(),
                    oid: *oid,
                }),
                Node::Dir(sub) => {
                    let sub_oid = materialize_dir(sub, out_trees);
                    entries.push(TreeEntry {
                        mode: "40000".to_string(),
                        name: name.clone(),
                        oid: sub_oid,
                    });
                }
            }
        }
        let tree_obj = build_tree_object(&entries);
        let oid = tree_obj.oid;
        out_trees.push(tree_obj);
        oid
    }

    let mut root = BTreeMap::new();
    for (path, oid) in files {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if !parts.is_empty() {
            insert_path(&mut root, &parts, *oid);
        }
    }
    let mut created_trees = Vec::new();
    let root_oid = materialize_dir(&root, &mut created_trees);
    (root_oid, created_trees)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pack_encode_decode_and_tree_synthesis() {
        let blob = GitObject::new(GitObjectType::Blob, Bytes::from_static(b"fn main() {}\n"));
        let mut file_map = BTreeMap::new();
        file_map.insert("src/main.rs".to_string(), blob.oid);
        let (root_tree_oid, mut trees) = build_trees_from_file_map(&file_map);
        let commit = build_commit_object(
            root_tree_oid,
            &[],
            "Codex Agent <agent@openai.com>",
            1793664000,
            "Initial agent commit",
        );

        let mut all_objs = vec![blob.clone()];
        all_objs.append(&mut trees);
        all_objs.push(commit.clone());

        let pack_bytes = encode_packfile(&all_objs);
        let decoded = decode_and_validate_pack(&pack_bytes, |_| None).unwrap();
        assert_eq!(decoded.objects.len(), 4);

        let obj_map: HashMap<ObjectId, GitObject> = decoded
            .objects
            .into_iter()
            .map(|o| (o.oid, o))
            .collect();
        let extracted = extract_commit_files(&commit.oid, &|oid| obj_map.get(oid).cloned()).unwrap();
        assert_eq!(extracted.get("src/main.rs"), Some(&blob.oid));
    }
}
