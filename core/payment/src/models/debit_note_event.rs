use crate::error::{DbError, DbResult};
use crate::schema::pay_debit_note_event;
use crate::utils::{json_from_str, json_to_string};
use chrono::{NaiveDateTime, TimeZone, Utc};
use serde::Serialize;
use std::convert::{TryFrom, TryInto};
use ya_client_model::payment::{DebitNoteEvent, EventType};
use ya_client_model::NodeId;

#[derive(Debug, Identifiable, Insertable)]
#[table_name = "pay_debit_note_event"]
#[primary_key(debit_note_id, event_type)]
pub struct WriteObj {
    pub debit_note_id: String,
    pub owner_id: NodeId,
    pub event_type: String,
    pub details: Option<String>,
}

impl WriteObj {
    pub fn new<T: Serialize>(
        debit_note_id: String,
        owner_id: NodeId,
        event_type: EventType,
        details: Option<T>,
    ) -> DbResult<Self> {
        let details = match details {
            Some(details) => Some(json_to_string(&details)?),
            None => None,
        };
        Ok(Self {
            debit_note_id,
            owner_id,
            event_type: event_type.into(),
            details,
        })
    }
}

#[derive(Queryable, Debug, Identifiable)]
#[table_name = "pay_debit_note_event"]
#[primary_key(debit_note_id, event_type)]
pub struct ReadObj {
    pub debit_note_id: String,
    pub owner_id: NodeId,
    pub event_type: String,
    pub timestamp: NaiveDateTime,
    pub details: Option<String>,
}

impl TryFrom<ReadObj> for DebitNoteEvent {
    type Error = DbError;

    fn try_from(event: ReadObj) -> DbResult<Self> {
        let details = match event.details {
            Some(s) => Some(json_from_str(&s)?),
            None => None,
        };
        Ok(Self {
            debit_note_id: event.debit_note_id,
            timestamp: Utc.from_utc_datetime(&event.timestamp),
            details,
            event_type: event.event_type.try_into()?,
        })
    }
}
