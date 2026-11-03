use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("invalid object id hex '{0}': expected 40 hex chars")]
    InvalidObjectId(String),
}

/// 20-byte Git SHA-1 Object ID.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId([u8; 20]);

impl ObjectId {
    pub const ZERO: ObjectId = ObjectId([0u8; 20]);

    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(slice: &[u8]) -> Result<Self, ProtoError> {
        if slice.len() != 20 {
            return Err(ProtoError::InvalidObjectId(hex::encode(slice)));
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(slice);
        Ok(Self(arr))
    }

    pub fn from_hex(s: &str) -> Result<Self, ProtoError> {
        let s = s.trim();
        if s.len() != 40 {
            return Err(ProtoError::InvalidObjectId(s.to_string()));
        }
        let decoded = hex::decode(s).map_err(|_| ProtoError::InvalidObjectId(s.to_string()))?;
        Self::from_slice(&decoded)
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 20]
    }
}

impl Default for ObjectId {
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", &self.to_hex()[..12])
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl FromStr for ObjectId {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl Serialize for ObjectId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        ObjectId::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Content-addressed identifier for an immutable Packfile stored in Blob Storage.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PackId(pub String);

impl PackId {
    pub fn from_pack_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        Self(format!("pack-{}", &hex::encode(digest)[..32]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Atomic pointer per branch — the ONLY contended key on a push (per-ref CAS).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefPointer {
    pub repo_id: String,
    pub ref_name: String,
    pub current_version: u64,
    pub tip_commit_oid: ObjectId,
    pub base_trunk_version: Option<u64>,
}

/// First-class agent provenance and verification metadata attached to a RefVersion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIntentReceipt {
    pub agent_id: String,
    pub model_id: String,
    pub prompt_summary: String,
    pub verified_tests: Vec<String>,
    pub timestamp_ms: u64,
}

/// Immutable record created on every push to a ref in Pando/Mesh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefVersion {
    pub repo_id: String,
    pub ref_name: String,
    pub version: u64,
    pub old_commit_oid: ObjectId,
    pub new_commit_oid: ObjectId,
    pub l0_pack_id: PackId,
    pub explicit_haves: Vec<ObjectId>,
    pub base_trunk_version: Option<u64>,
    pub created_at_ms: u64,
    pub intent_receipt: Option<AgentIntentReceipt>,
    pub semantic_deltas: Vec<FileSemanticDelta>,
}

/// Inverted index mapping commit OIDs to the RefVersion that introduced them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitLocation {
    pub repo_id: String,
    pub commit_oid: ObjectId,
    pub ref_name: String,
    pub ref_version: u64,
}

/// LSM-style Compaction Cover spanning `[start_version, end_version]` on a ref.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackCover {
    pub repo_id: String,
    pub ref_name: String,
    pub level: u8,
    pub start_version: u64,
    pub end_version: u64,
    pub pack_id: PackId,
    pub byte_size: u64,
    pub object_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Import,
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Class,
    Interface,
    TypeAlias,
    Const,
    Static,
    Module,
    Macro,
    VerbatimBlock,
}

/// Concrete Syntax Tree (CST) span for a top-level declaration in a source file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolSpan {
    pub name: String,
    pub kind: SymbolKind,
    pub signature: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub body_hash: String,
    pub referenced_symbols: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeOperation {
    Inserted {
        signature: String,
    },
    Modified {
        old_signature: String,
        new_signature: String,
        signature_changed: bool,
    },
    Deleted {
        old_signature: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolDelta {
    pub symbol_name: String,
    pub kind: SymbolKind,
    pub op: ChangeOperation,
    pub referenced_symbols: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSemanticDelta {
    pub file_path: String,
    pub language: String,
    pub old_blob_oid: ObjectId,
    pub new_blob_oid: ObjectId,
    pub symbol_deltas: Vec<SymbolDelta>,
    pub imports: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SemanticConflictKind {
    /// Both branches mutated the body/signature of the exact same AST symbol.
    SameSymbolModified,
    /// Branch A changed the signature of symbol `S`, while Branch B added/modified a call to `S`.
    SignatureCallMismatch,
    /// One branch modified a file while the other branch deleted it.
    FileModifyDeleteConflict,
    /// File could not be parsed cleanly into CST declarations and had overlapping line edits.
    UnstructuredOverlapFallback,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticConflict {
    pub file_path: String,
    pub symbol_name: String,
    pub branch_a: String,
    pub branch_b: String,
    pub kind: SemanticConflictKind,
    pub description: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_object_id_roundtrip() {
        let hex = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let oid = ObjectId::from_hex(hex).unwrap();
        assert_eq!(oid.to_hex(), hex);
        assert!(!oid.is_zero());
        assert!(ObjectId::ZERO.is_zero());
    }
}
