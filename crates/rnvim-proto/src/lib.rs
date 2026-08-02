//! Wire protocol between the rnvim client and the remote agent.
//!
//! Transport: newline-delimited JSON over a byte stream (agent stdio, or the
//! local session unix socket). One request line yields exactly one response
//! line with the same `id`.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible protocol change. Client and agent must match.
pub const PROTO_VERSION: u32 = 3;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl Response {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Response {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, code: i32, message: impl Into<String>) -> Self {
        Response {
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

pub const ERR_PARSE: i32 = -32700;
pub const ERR_UNKNOWN_METHOD: i32 = -32601;
pub const ERR_IO: i32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct HelloParams {
    pub client_version: String,
    pub proto: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HelloResult {
    pub agent_version: String,
    pub proto: u32,
    pub os: String,
    pub arch: String,
    pub home: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResolveParams {
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResolveResult {
    pub abs: String,
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatParams {
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatResult {
    pub kind: String,
    pub size: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReadParams {
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReadResult {
    pub content_b64: String,
    pub size: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WriteParams {
    pub path: String,
    pub content_b64: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WriteResult {
    pub bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListParams {
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListResult {
    pub entries: Vec<DirEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FindrootParams {
    pub path: String,
    pub markers: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FindrootResult {
    /// Nearest ancestor directory containing any marker, if one exists.
    pub root: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WhichParams {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WhichResult {
    pub path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FindFilesParams {
    pub root: String,
    pub query: String,
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FindFilesResult {
    /// Paths relative to `root`, best matches first.
    pub files: Vec<String>,
    /// Total files under root (after ignore rules), for "N of M" display.
    pub total: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GrepParams {
    pub root: String,
    pub query: String,
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GrepMatch {
    pub path: String,
    pub line: u64,
    pub col: u64,
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub truncated: bool,
}
