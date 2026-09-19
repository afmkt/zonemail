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

impl From<String> for Domain {
    fn from(id: String) -> Self {
        Domain {
            id,
            created_at: jiff::Timestamp::now(),
            updated_at: jiff::Timestamp::now(),
            mailboxes: Deferred::default(),
            records: Deferred::default(),
        }
    }
}
impl From<&String> for Domain {
    fn from(id: &String) -> Self {
        id.as_str().into() // Delegates to From<&str>
    }
}
impl From<&str> for Domain {
    fn from(id: &str) -> Self {
        id.to_string().into()
    }
}

impl From<Domain> for String {
    fn from(domain: Domain) -> Self {
        domain.id
    }
}

impl From<&Domain> for String {
    fn from(domain: &Domain) -> Self {
        domain.id.clone()
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordDTO {
    pub domain_id: String,
    pub name: String,
    pub record_type: RecordType,
    pub value: String,
    pub ttl: u32,
}

impl From<RecordDTO> for Record {
    fn from(dto: RecordDTO) -> Self {
        Record {
            id: 0, // This will be auto-generated
            domain_id: dto.domain_id,
            name: dto.name,
            record_type: dto.record_type,
            value: dto.value,
            ttl: dto.ttl,
            created_at: jiff::Timestamp::now(),
            updated_at: jiff::Timestamp::now(),
            domain: Deferred::default(),
        }
    }
}
impl From<&RecordDTO> for Record {
    fn from(dto: &RecordDTO) -> Self {
        dto.clone().into()
    }
}
impl From<Record> for RecordDTO {
    fn from(record: Record) -> Self {
        RecordDTO {
            domain_id: record.domain_id,
            name: record.name,
            record_type: record.record_type,
            value: record.value,
            ttl: record.ttl,
        }
    }
}
impl From<&Record> for RecordDTO {
    fn from(value: &Record) -> Self {
        value.into()
    }
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
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MailboxError {
    #[error("Invalid email address format (missing domain)")]
    InvalidFormat,
}
impl TryFrom<String> for Mailbox {
    type Error = MailboxError;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        // Ensure there is a domain part after the '@'
        let domain_id = id
            .split('@')
            .nth(1)
            .ok_or(MailboxError::InvalidFormat)?
            .to_string();

        if domain_id.is_empty() {
            return Err(MailboxError::InvalidFormat);
        }

        Ok(Mailbox {
            id,
            domain_id,
            created_at: jiff::Timestamp::now(),
            updated_at: jiff::Timestamp::now(),
            domain: Deferred::default(),
            outbound: Deferred::default(),
            inbound: Deferred::default(),
        })
    }
}

impl TryFrom<&String> for Mailbox {
    type Error = MailboxError;
    fn try_from(id: &String) -> Result<Self, Self::Error> {
        id.clone().try_into() // Delegates to TryFrom<&str>
    }
}
impl TryFrom<&str> for Mailbox {
    type Error = MailboxError;

    fn try_from(id: &str) -> Result<Self, Self::Error> {
        id.to_string().try_into()
    }
}
impl From<Mailbox> for String {
    fn from(mailbox: Mailbox) -> Self {
        mailbox.id
    }
}

impl From<&Mailbox> for String {
    fn from(mailbox: &Mailbox) -> Self {
        mailbox.id.clone()
    }
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
