use serde::{Deserialize, Serialize};
use toasty::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, toasty::Embed)]
pub enum RecordType {
    A,
    AAAA,
    CNAME,
    MX,
    NS,
    SOA,
    TXT,
    CAA,
    SRV,
    PTR,
    SVCB,
    HTTPS,
}

#[derive(Model)]
pub struct Domain {
    #[key]
    pub id: String,

    #[auto]
    pub created_at: jiff::Timestamp,
    #[auto]
    pub updated_at: jiff::Timestamp,

    #[has_many]
    pub mailboxes: Deferred<Vec<Mailbox>>,
    #[has_many]
    pub records: Deferred<Vec<Record>>,
}

#[derive(Model)]
#[unique(domain_id, name, record_type, value)]
pub struct Record {
    #[key]
    #[auto]
    pub id: u64,

    #[index]
    pub domain_id: String,
    pub name: String,
    pub record_type: RecordType,
    pub value: String,
    pub ttl: u32,

    #[auto]
    pub created_at: jiff::Timestamp,
    #[auto]
    pub updated_at: jiff::Timestamp,

    #[belongs_to]
    pub domain: Deferred<Domain>,
}

#[derive(Model)]
pub struct Mailbox {
    #[key]
    pub id: String, // e.g., "user@domain.com"

    #[index]
    pub domain_id: String,

    #[auto]
    pub created_at: jiff::Timestamp,
    #[auto]
    pub updated_at: jiff::Timestamp,

    #[belongs_to]
    pub domain: Deferred<Domain>,

    #[has_many]
    pub outbound: Deferred<Vec<OutboundMessage>>,
    #[has_many]
    pub inbound: Deferred<Vec<InboundMessage>>,
}

#[derive(Model)]
pub struct InboundMessage {
    #[key]
    #[auto]
    pub id: u64,
    pub envelope_from: String,

    #[index]
    pub recipient_id: String,

    pub subject: Option<String>,
    pub body: Option<String>,

    #[auto]
    pub created_at: jiff::Timestamp,
    #[auto]
    pub updated_at: jiff::Timestamp,

    #[belongs_to(key = recipient_id, references = id)]
    pub mailbox: Deferred<Mailbox>,

    #[has_many]
    pub delivery_statuses: Deferred<Vec<DeliveryStatus>>,
}

#[derive(Model)]
pub struct OutboundMessage {
    #[key]
    #[auto]
    pub id: u64,

    #[index]
    pub sender_id: String,

    pub envelope_to: String,
    pub subject: Option<String>,
    pub body: Option<String>,

    #[auto]
    pub created_at: jiff::Timestamp,
    #[auto]
    pub updated_at: jiff::Timestamp,

    #[belongs_to(key = sender_id, references = id)]
    pub mailbox: Deferred<Mailbox>,

    #[has_many]
    pub delivery_statuses: Deferred<Vec<DeliveryStatus>>,
}

#[derive(Model)]
pub struct DeliveryStatus {
    #[key]
    #[auto]
    pub id: u64,

    #[index]
    pub outbound_message_id: Option<u64>,
    #[index]
    pub inbound_message_id: Option<u64>,

    pub peer_host: String,
    pub code: Option<u64>,
    pub message: Option<String>,

    #[auto]
    pub created_at: jiff::Timestamp,
    #[auto]
    pub updated_at: jiff::Timestamp,

    #[belongs_to]
    pub outbound_message: Deferred<OutboundMessage>,
    #[belongs_to]
    pub inbound_message: Deferred<InboundMessage>,
}
