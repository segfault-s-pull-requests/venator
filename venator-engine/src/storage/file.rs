use rusqlite::{Connection, Error as DbError, Params, Row};

use crate::{Event, Instance, Span, SpanEvent, SpanEventKind, Timestamp};

use super::{Boo, Storage};

pub struct FileStorage {
    connection: Connection,
}

impl FileStorage {
    pub fn new(path: &str) -> FileStorage {
        let connection = Connection::open(path).unwrap();

        connection
            .execute_batch(r#"PRAGMA synchronous = OFF; PRAGMA journal_mode = OFF;"#)
            .unwrap();

        let _ = connection.execute_batch(
            r#"
            CREATE TABLE instances (
                key             INT8 NOT NULL,
                id              INT8,
                disconnected_at INT8,
                fields          TEXT,

                CONSTRAINT instances_pk PRIMARY KEY (key)
            );

            CREATE TABLE spans (
                key       INT8 NOT NULL,
                instance  INT8,
                id        INT8,
                closed_at INT8,
                parent_id INT8,
                target    TEXT,
                name      TEXT,
                level     INT,
                file_name TEXT,
                file_line INTEGER,
                fields    TEXT,

                CONSTRAINT spans_pk PRIMARY KEY (key)
            );

            CREATE TABLE span_events (
                key       INT8 NOT NULL,
                instance  INT8,
                span_id   INT8,
                kind      TEXT,
                data      TEXT,

                CONSTRAINT span_events_pk PRIMARY KEY (key)
            );

            CREATE TABLE events (
                key       INT8 NOT NULL,
                instance  INT8,
                span_id   INT8,
                target    TEXT,
                name      TEXT,
                level     INT,
                file_name TEXT,
                file_line INTEGER,
                fields    TEXT,

                CONSTRAINT events_pk PRIMARY KEY (key)
            );
        "#,
        );

        FileStorage { connection }
    }
}

impl Storage for FileStorage {
    fn get_instance(&self, at: Timestamp) -> Option<Boo<'_, Instance>> {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT * FROM instances WHERE key = ?1")
            .unwrap();

        let result = stmt.query_row((at,), instance_from_row);

        Some(Boo::Owned(result.unwrap()))
    }

    fn get_span(&self, at: Timestamp) -> Option<Boo<'_, Span>> {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT * FROM spans WHERE key = ?1")
            .unwrap();

        let result = stmt.query_row((at,), span_from_row);

        Some(Boo::Owned(result.unwrap()))
    }

    fn get_span_event(&self, at: Timestamp) -> Option<Boo<'_, SpanEvent>> {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT * FROM span_events WHERE key = ?1")
            .unwrap();

        let result = stmt.query_row((at,), span_event_from_row);

        Some(Boo::Owned(result.unwrap()))
    }

    fn get_event(&self, at: Timestamp) -> Option<Boo<'_, Event>> {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT * FROM events WHERE key = ?1")
            .unwrap();

        let result = stmt.query_row((at,), event_from_row);

        Some(Boo::Owned(result.unwrap()))
    }

    fn get_all_instances(&self) -> Box<dyn Iterator<Item = Boo<'_, Instance>> + '_> {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT * FROM instances ORDER BY key")
            .unwrap();

        let instances = stmt
            .query_map((), instance_from_row)
            .unwrap()
            .map(|result| result.unwrap())
            .collect::<Vec<_>>();

        Box::new(instances.into_iter().map(Boo::Owned))
    }

    fn get_all_spans(&self) -> Box<dyn Iterator<Item = Boo<'_, Span>> + '_> {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT * FROM spans ORDER BY key")
            .unwrap();

        let spans = stmt
            .query_map((), span_from_row)
            .unwrap()
            .map(|result| result.unwrap())
            .collect::<Vec<_>>();

        Box::new(spans.into_iter().map(Boo::Owned))
    }

    fn get_all_span_events(&self) -> Box<dyn Iterator<Item = Boo<'_, SpanEvent>> + '_> {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT * FROM span_events ORDER BY key")
            .unwrap();

        let span_events = stmt
            .query_map((), span_event_from_row)
            .unwrap()
            .map(|result| result.unwrap())
            .collect::<Vec<_>>();

        Box::new(span_events.into_iter().map(Boo::Owned))
    }

    fn get_all_events(&self) -> Box<dyn Iterator<Item = Boo<'_, Event>> + '_> {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT * FROM events ORDER BY key")
            .unwrap();

        let events = stmt
            .query_map((), event_from_row)
            .unwrap()
            .map(|result| result.unwrap())
            .collect::<Vec<_>>();

        Box::new(events.into_iter().map(Boo::Owned))
    }

    fn insert_instance(&mut self, instance: Instance) {
        let mut stmt = self
            .connection
            .prepare_cached("INSERT INTO instances VALUES (?1, ?2, ?3, ?4)")
            .unwrap();

        stmt.execute(instance_to_params(instance)).unwrap();
    }

    fn insert_span(&mut self, span: Span) {
        let mut stmt = self
            .connection
            .prepare_cached(
                "INSERT INTO spans VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )
            .unwrap();

        stmt.execute(span_to_params(span)).unwrap();
    }

    fn insert_span_event(&mut self, span_event: SpanEvent) {
        let mut stmt = self
            .connection
            .prepare_cached("INSERT INTO span_events VALUES (?1, ?2, ?3, ?4, ?5)")
            .unwrap();

        stmt.execute(span_event_to_params(span_event)).unwrap();
    }

    fn insert_event(&mut self, event: Event) {
        let mut stmt = self
            .connection
            .prepare_cached("INSERT INTO events VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)")
            .unwrap();

        stmt.execute(event_to_params(event)).unwrap();
    }

    fn update_instance_disconnected(&mut self, at: Timestamp, disconnected: Timestamp) {
        let mut stmt = self
            .connection
            .prepare_cached("UPDATE instances SET disconnected_at = ?2 WHERE key = ?1")
            .unwrap();

        stmt.execute((at, disconnected)).unwrap();
    }

    fn update_span_closed(&mut self, at: Timestamp, closed: Timestamp) {
        let mut stmt = self
            .connection
            .prepare_cached("UPDATE spans SET closed_at = ?2 WHERE key = ?1")
            .unwrap();

        stmt.execute((at, closed)).unwrap();
    }

    fn update_span_fields(
        &mut self,
        at: Timestamp,
        fields: std::collections::BTreeMap<String, String>,
    ) {
        let mut stmt = self
            .connection
            .prepare_cached("SELECT * FROM spans WHERE spans.key = ?1")
            .unwrap();

        let span = stmt.query_row((at,), span_from_row).unwrap();
        let existing_fields = span.fields;

        let fields = {
            let mut new_fields = existing_fields;
            new_fields.extend(fields);
            new_fields
        };
        let fields = serde_json::to_string(&fields).unwrap();

        let mut stmt = self
            .connection
            .prepare_cached("UPDATE spans SET fields = ?2 WHERE key = ?1")
            .unwrap();

        stmt.execute((at, fields)).unwrap();
    }
}

fn instance_to_params(instance: Instance) -> impl Params {
    let key = instance.key();
    let id = instance.id;
    let disconnected_at = instance.disconnected_at;
    let fields = serde_json::to_string(&instance.fields).unwrap();

    (key, id as i64, disconnected_at, fields)
}

fn instance_from_row(row: &Row<'_>) -> Result<Instance, DbError> {
    let key = row.get(0)?;
    let id: i64 = row.get(1)?;
    let disconnected_at = row.get(2)?;
    let fields: String = row.get(3)?;
    let fields = serde_json::from_str(&fields).unwrap();

    Ok(Instance {
        id: id as u64,
        connected_at: key,
        disconnected_at,
        fields,
    })
}

#[rustfmt::skip]
fn span_to_params(span: Span) -> impl Params {
    let key = span.created_at;
    let instance_key = span.instance_key;
    let id = span.id as i64;
    let closed_at = span.closed_at;
    let parent_id = span.parent_key;
    let target = span.target;
    let name = span.name;
    let level = span.level as i32;
    let file_name = span.file_name;
    let file_line = span.file_line;
    let fields = serde_json::to_string(&span.fields).unwrap();

    (key, instance_key, id, closed_at, parent_id, target, name, level, file_name, file_line, fields)
}

fn span_from_row(row: &Row<'_>) -> Result<Span, DbError> {
    let key = row.get(0)?;
    let instance_key = row.get(1)?;
    let id: i64 = row.get(2)?;
    let closed_at = row.get(3)?;
    let parent_key = row.get(4)?;
    let target = row.get(5)?;
    let name = row.get(6)?;
    let level: i32 = row.get(7)?;
    let file_name = row.get(8)?;
    let file_line = row.get(9)?;
    let fields: String = row.get(10)?;
    let fields = serde_json::from_str(&fields).unwrap();

    Ok(Span {
        created_at: key,
        instance_key,
        id: id as u64,
        closed_at,
        parent_key,
        target,
        name,
        level: level.try_into().unwrap(),
        file_name,
        file_line,
        fields,
    })
}

fn span_event_to_params(span_event: SpanEvent) -> impl Params {
    match span_event.kind {
        SpanEventKind::Create(create_span_event) => {
            let key = span_event.timestamp;
            let instance_key = span_event.instance_key;
            let span_key = span_event.span_key;
            let kind = "create";
            let data = serde_json::to_string(&create_span_event).unwrap();

            (key, instance_key, span_key, kind, Some(data))
        }
        SpanEventKind::Update(update_span_event) => {
            let key = span_event.timestamp;
            let instance_key = span_event.instance_key;
            let span_key = span_event.span_key;
            let kind = "update";
            let data = serde_json::to_string(&update_span_event).unwrap();

            (key, instance_key, span_key, kind, Some(data))
        }
        SpanEventKind::Enter => {
            let key = span_event.timestamp;
            let instance_key = span_event.instance_key;
            let span_key = span_event.span_key;
            let kind = "enter";

            (key, instance_key, span_key, kind, None)
        }
        SpanEventKind::Exit => {
            let key = span_event.timestamp;
            let instance_key = span_event.instance_key;
            let span_key = span_event.span_key;
            let kind = "exit";

            (key, instance_key, span_key, kind, None)
        }
        SpanEventKind::Close => {
            let key = span_event.timestamp;
            let instance_key = span_event.instance_key;
            let span_key = span_event.span_key;
            let kind = "close";

            (key, instance_key, span_key, kind, None)
        }
    }
}

fn span_event_from_row(row: &Row<'_>) -> Result<SpanEvent, DbError> {
    let key = row.get(0)?;
    let instance_key = row.get(1)?;
    let span_key = row.get(2)?;
    let kind: String = row.get(3)?;
    let data: Option<String> = row.get(4)?;
    match kind.as_str() {
        "create" => {
            let create_span_event = serde_json::from_str(&data.unwrap()).unwrap();
            Ok(SpanEvent {
                instance_key,
                timestamp: key,
                span_key,
                kind: SpanEventKind::Create(create_span_event),
            })
        }
        "update" => {
            let update_span_event = serde_json::from_str(&data.unwrap()).unwrap();
            Ok(SpanEvent {
                instance_key,
                timestamp: key,
                span_key,
                kind: SpanEventKind::Update(update_span_event),
            })
        }
        "enter" => Ok(SpanEvent {
            instance_key,
            timestamp: key,
            span_key,
            kind: SpanEventKind::Enter,
        }),
        "exit" => Ok(SpanEvent {
            instance_key,
            timestamp: key,
            span_key,
            kind: SpanEventKind::Exit,
        }),
        "close" => Ok(SpanEvent {
            instance_key,
            timestamp: key,
            span_key,
            kind: SpanEventKind::Close,
        }),
        _ => panic!("unknown span event kind"),
    }
}

#[rustfmt::skip]
fn event_to_params(event: Event) -> impl Params {
    let key = event.timestamp;
    let instance_key = event.instance_key;
    let span_key = event.span_key;
    let target = event.target;
    let name = event.name;
    let level = event.level as i32;
    let file_name = event.file_name;
    let file_line = event.file_line;
    let fields = serde_json::to_string(&event.fields).unwrap();

    (key, instance_key, span_key, target, name, level, file_name, file_line, fields)
}

fn event_from_row(row: &Row<'_>) -> Result<Event, DbError> {
    let key = row.get(0)?;
    let instance_key = row.get(1)?;
    let span_key = row.get(2)?;
    let target = row.get(3)?;
    let name = row.get(4)?;
    let level: i32 = row.get(5)?;
    let file_name = row.get(6)?;
    let file_line = row.get(7)?;
    let fields: String = row.get(8)?;
    let fields = serde_json::from_str(&fields).unwrap();

    Ok(Event {
        timestamp: key,
        instance_key,
        span_key,
        target,
        name,
        level: level.try_into().unwrap(),
        file_name,
        file_line,
        fields,
    })
}