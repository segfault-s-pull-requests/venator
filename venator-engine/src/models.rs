use std::collections::BTreeMap;
use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

pub type Timestamp = NonZeroU64;

/// This is the internal type used to identify instances. The value is the
/// unique timestamp from when the instance was created.
pub type InstanceKey = NonZeroU64;

/// This is the external type used to identity an instance. This is generated
/// client-side and should be random to make it unique.
pub type InstanceId = u64;

/// This is the internal type used to identify spans. The value is the unique
/// timestamp from when the span was created.
pub type SpanKey = NonZeroU64;

/// This is the internal type used to identify span eventss. The value is the
/// semi-unique timestamp from when the span event was created. "Semi-unique"
/// because the "create" event shares a timestamp with the span it creates.
pub type SpanEventKey = NonZeroU64;

/// This is the internal type used to identify events. The value is the unique
/// timestamp from when the event was created.
pub type EventKey = NonZeroU64;

/// This is the external type used to identity a span. This is generated client-
/// side and is unique but only within that instance.
pub type SpanId = u64;

pub type InstanceIdView = String;
pub type FullSpanIdView = String;

pub type FullSpanId = (InstanceId, SpanId);

pub type SubscriptionId = usize;

pub fn parse_full_span_id(s: &str) -> Option<FullSpanId> {
    let (instance_id, span_id) = s.split_once('-')?;
    let instance_id: InstanceId = instance_id.parse().ok()?;
    let span_id: SpanId = span_id.parse().ok()?;

    Some((instance_id, span_id))
}

#[derive(
    Debug, Copy, Clone, PartialEq, Eq, serde_repr::Serialize_repr, serde_repr::Deserialize_repr,
)]
#[repr(i32)]
pub enum Level {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl TryFrom<i32> for Level {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, ()> {
        match value {
            0 => Ok(Level::Trace),
            1 => Ok(Level::Debug),
            2 => Ok(Level::Info),
            3 => Ok(Level::Warn),
            4 => Ok(Level::Error),
            _ => Err(()),
        }
    }
}

pub struct NewInstance {
    pub id: InstanceId,
    pub fields: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct Instance {
    pub id: InstanceId,
    pub connected_at: Timestamp,
    pub disconnected_at: Option<Timestamp>,
    pub fields: BTreeMap<String, String>,
}

impl Instance {
    pub fn key(&self) -> InstanceKey {
        self.connected_at
    }

    // gets the duration of the span in microseconds if disconnected
    pub fn duration(&self) -> Option<u64> {
        self.disconnected_at.map(|disconnected_at| {
            disconnected_at
                .get()
                .saturating_sub(self.connected_at.get())
        })
    }
}

#[derive(Serialize)]
pub struct InstanceView {
    pub id: InstanceIdView,
    pub connected_at: Timestamp,
    pub disconnected_at: Option<Timestamp>,
    pub attributes: Vec<AttributeView>,
}

pub struct NewSpanEvent {
    pub instance_key: InstanceKey,
    pub timestamp: Timestamp,
    pub span_id: SpanId,
    pub kind: NewSpanEventKind,
}

pub enum NewSpanEventKind {
    Create(NewCreateSpanEvent),
    Update(NewUpdateSpanEvent),
    Follows(NewFollowsSpanEvent),
    Enter,
    Exit,
    Close,
}

#[derive(Clone)]
pub struct SpanEvent {
    pub instance_key: InstanceKey,
    pub timestamp: Timestamp,
    pub span_key: SpanKey,
    pub kind: SpanEventKind,
}

#[derive(Clone)]
pub enum SpanEventKind {
    Create(CreateSpanEvent),
    Update(UpdateSpanEvent),
    Enter,
    Exit,
    Close,
}

pub struct NewCreateSpanEvent {
    pub parent_id: Option<SpanId>,
    pub target: String,
    pub name: String,
    pub level: i32,
    pub file_name: Option<String>,
    pub file_line: Option<u32>,
    pub fields: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CreateSpanEvent {
    pub parent_key: Option<SpanKey>,
    pub target: String,
    pub name: String,
    pub level: Level,
    pub file_name: Option<String>,
    pub file_line: Option<u32>,
    pub fields: BTreeMap<String, String>,
}

pub struct NewUpdateSpanEvent {
    pub fields: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct UpdateSpanEvent {
    pub fields: BTreeMap<String, String>,
}

pub struct NewFollowsSpanEvent {
    pub follows: SpanId,
}

pub struct NewEvent {
    pub instance_key: InstanceKey,
    pub timestamp: Timestamp,
    pub span_id: Option<SpanId>,
    pub name: String,
    pub target: String,
    pub level: i32,
    pub file_name: Option<String>,
    pub file_line: Option<u32>,
    pub fields: BTreeMap<String, String>,
}

#[derive(Clone, Serialize)]
pub struct Event {
    pub instance_key: InstanceKey,
    pub timestamp: Timestamp,
    pub span_key: Option<SpanKey>,
    pub name: String,
    pub target: String,
    pub level: Level,
    pub file_name: Option<String>,
    pub file_line: Option<u32>,
    pub fields: BTreeMap<String, String>,
}

impl Event {
    pub fn key(&self) -> EventKey {
        self.timestamp
    }
}

#[derive(Clone, Serialize)]
pub struct EventView {
    pub instance_id: InstanceIdView,
    pub ancestors: Vec<AncestorView>,
    pub timestamp: Timestamp,
    pub target: String,
    pub name: String,
    pub level: i32,
    pub file: Option<String>,
    pub attributes: Vec<AttributeView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Span {
    pub instance_key: InstanceKey,
    pub id: SpanId,
    pub created_at: Timestamp,
    pub closed_at: Option<Timestamp>,
    pub parent_key: Option<SpanKey>,
    pub target: String,
    pub name: String,
    pub level: Level,
    pub file_name: Option<String>,
    pub file_line: Option<u32>,
    pub fields: BTreeMap<String, String>,
}

impl Span {
    pub fn key(&self) -> SpanKey {
        self.created_at
    }
}

#[derive(Serialize)]
pub struct SpanView {
    pub id: FullSpanIdView,
    pub ancestors: Vec<AncestorView>,
    pub created_at: Timestamp,
    pub closed_at: Option<Timestamp>,
    pub target: String,
    pub name: String,
    pub level: i32,
    pub file: Option<String>,
    pub attributes: Vec<AttributeView>,
}

#[derive(Clone, Serialize)]
pub struct AncestorView {
    pub id: FullSpanIdView,
    pub name: String,
}

#[derive(Clone, Serialize)]
pub struct AttributeView {
    pub name: String,
    pub value: String,
    #[serde(flatten)]
    pub kind: AttributeKindView,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AttributeKindView {
    Instance { instance_id: InstanceId },
    Span { span_id: FullSpanId },
    Inherent,
}

impl Span {
    // gets the duration of the span in microseconds if closed
    pub fn duration(&self) -> Option<u64> {
        self.closed_at
            .map(|closed_at| closed_at.get().saturating_sub(self.created_at.get()))
    }
}

#[derive(Serialize)]
pub struct StatsView {
    pub start: Option<Timestamp>,
    pub end: Option<Timestamp>,
    pub total_spans: usize,
    pub total_events: usize,
}