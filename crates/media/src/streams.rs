//! Persistent container stream identities and presentation metadata.
use crate::{AudioStreamInfo, VideoStreamInfo};
use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};

/// IDs are strings so 64-bit Matroska UIDs survive JSON clients exactly.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "container", rename_all = "camelCase", deny_unknown_fields)]
pub enum StreamId {
    Mp4 { track: u32 },
    Matroska { uid: String, number: String },
}

pub const MAX_CONTAINER_STREAMS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamKind {
    Video,
    Audio,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub id: StreamId,
    pub kind: StreamKind,
    pub codec: String,
    pub name: String,
    pub timebase: (u64, u64),
    /// Raw earliest presentation timestamp, before clamping codec priming to zero.
    pub first_pts: i64,
    pub start: Tick,
    pub end: Tick,
    pub video: Option<VideoStreamInfo>,
    pub audio: Option<AudioStreamInfo>,
    /// Channel labels in decode order; empty means unspecified by the container.
    pub channel_layout: Vec<String>,
    pub unsupported: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamCatalog {
    pub origin: Tick,
    pub streams: Vec<StreamInfo>,
}
