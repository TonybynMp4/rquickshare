use std::fs::File;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::TextPayloadType;
use crate::utils::RemoteDeviceInfo;

#[derive(Debug, Default)]
pub struct InternalFileInfo {
    pub payload_id: i64,
    /// Inbound: the sanitized name the file is saved under (before any 1_
    /// prefix), and its slot in `saved_files`. Unused outbound.
    pub name: String,
    pub saved_index: usize,
    /// Inbound: empty until the file is created at accept time.
    pub file_url: PathBuf,
    pub bytes_transferred: i64,
    pub total_size: i64,
    pub file: Option<File>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, TS)]
#[ts(export)]
pub struct TransferMetadata {
    pub id: String,
    pub source: Option<RemoteDeviceInfo>,
    pub pin_code: Option<String>,

    pub destination: Option<String>,
    pub files: Option<Vec<String>>,

    pub text_type: Option<TextPayloadType>,
    pub text_description: Option<String>,
    pub text_payload: Option<String>,

    pub total_bytes: u64,
    pub ack_bytes: u64,
}
