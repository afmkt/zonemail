use hickory_proto::rr::RecordType as HickoryRecordType;
use serde::{Deserialize, Serialize};
use toasty::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, toasty::Embed)]
pub enum RecordType {
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035) IPv4 Address record
    A,
    /// [RFC 3596](https://tools.ietf.org/html/rfc3596) IPv6 address record
    AAAA,
    /// [ANAME draft-ietf-dnsop-aname](https://tools.ietf.org/html/draft-ietf-dnsop-aname-04)
    ANAME,
    //  AFSDB,      //	18	RFC 1183	AFS database record
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035) All cached records, aka ANY
    ANY,
    //  APL,        //	42	RFC 3123	Address Prefix List
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035) Authoritative Zone Transfer
    AXFR,
    /// [RFC 6844](https://tools.ietf.org/html/rfc6844) Certification Authority Authorization
    CAA,
    /// [RFC 7344](https://tools.ietf.org/html/rfc7344) Child DS
    CDS,
    /// [RFC 7344](https://tools.ietf.org/html/rfc7344) Child DNSKEY
    CDNSKEY,
    /// [RFC 4398](https://tools.ietf.org/html/rfc4398) Storing Certificates in the Domain Name System (DNS)
    CERT,
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035) Canonical name record
    CNAME,
    //  DHCID,      // 49 RFC 4701 DHCP identifier
    //  DLV,        //	32769	RFC 4431	DNSSEC Lookaside Validation record
    /// [RFC 6672](https://tools.ietf.org/html/rfc6672) Delegation Name (RData unsupported)
    DNAME,
    /// [RFC 7477](https://tools.ietf.org/html/rfc4034) Child-to-parent synchronization record
    CSYNC,
    /// [RFC 4034](https://tools.ietf.org/html/rfc4034) DNS Key record: RSASHA256 and RSASHA512, RFC5702
    DNSKEY,
    /// [RFC 4034](https://tools.ietf.org/html/rfc4034) Delegation signer: RSASHA256 and RSASHA512, RFC5702
    DS,
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035) host information
    HINFO,
    //  HIP,        // 55 RFC 5205 Host Identity Protocol
    /// [RFC 9460](https://tools.ietf.org/html/rfc9460) DNS SVCB and HTTPS RRs
    HTTPS,
    //  IPSECKEY,   // 45 RFC 4025 IPsec Key
    /// [RFC 1996](https://tools.ietf.org/html/rfc1996) Incremental Zone Transfer
    IXFR,
    //  KX,         // 36 RFC 2230 Key eXchanger record
    /// [RFC 2535](https://tools.ietf.org/html/rfc2535) and [RFC 2930](https://tools.ietf.org/html/rfc2930) Key record
    KEY,
    //  LOC,        // 29 RFC 1876 Location record
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035) Mail exchange record
    MX,
    /// [RFC 3403](https://tools.ietf.org/html/rfc3403) Naming Authority Pointer
    NAPTR,
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035) Name server record
    NS,
    /// [RFC 4034](https://tools.ietf.org/html/rfc4034) Next-Secure record
    NSEC,
    /// [RFC 5155](https://tools.ietf.org/html/rfc5155) NSEC record version 3
    NSEC3,
    /// [RFC 5155](https://tools.ietf.org/html/rfc5155) NSEC3 parameters
    NSEC3PARAM,
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035) Null server record, for testing
    NULL,
    /// [RFC 7929](https://tools.ietf.org/html/rfc7929) OpenPGP public key
    OPENPGPKEY,
    /// [RFC 6891](https://tools.ietf.org/html/rfc6891) Option
    OPT,
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035) Pointer record
    PTR,
    //  RP,         // 17 RFC 1183 Responsible person
    /// [RFC 4034](https://tools.ietf.org/html/rfc4034) DNSSEC signature: RSASHA256 and RSASHA512, RFC5702
    RRSIG,
    /// [RFC 2535](https://tools.ietf.org/html/rfc2535) (and [RFC 2931](https://tools.ietf.org/html/rfc2931)) Signature, to support [RFC 2137](https://tools.ietf.org/html/rfc2137) Update.
    SIG,
    /// [RFC 8162](https://datatracker.ietf.org/doc/html/rfc8162)
    SMIMEA,
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035) and [RFC 2308](https://tools.ietf.org/html/rfc2308) Start of [a zone of] authority record
    SOA,
    /// [RFC 2782](https://tools.ietf.org/html/rfc2782) Service locator
    SRV,
    /// [RFC 4255](https://tools.ietf.org/html/rfc4255) SSH Public Key Fingerprint
    SSHFP,
    /// [RFC 9460](https://tools.ietf.org/html/rfc9460) DNS SVCB and HTTPS RRs
    SVCB,
    //  TA,         // 32768 N/A DNSSEC Trust Authorities
    //  TKEY,       // 249 RFC 2930 Secret key record
    /// [RFC 6698](https://tools.ietf.org/html/rfc6698) TLSA certificate association
    TLSA,
    /// [RFC 8945](https://tools.ietf.org/html/rfc8945) Transaction Signature
    TSIG,
    /// [RFC 1035](https://tools.ietf.org/html/rfc1035) Text record
    TXT,
    /// Unknown Record type, or unsupported
    Unknown,

    /// This corresponds to a record type of 0, unspecified
    ZERO,
}

impl From<HickoryRecordType> for RecordType {
    fn from(value: HickoryRecordType) -> Self {
        match value {
            HickoryRecordType::A => RecordType::A,
            HickoryRecordType::AAAA => RecordType::AAAA,
            HickoryRecordType::ANAME => RecordType::ANAME,
            HickoryRecordType::ANY => RecordType::ANY,
            HickoryRecordType::AXFR => RecordType::AXFR,
            HickoryRecordType::CAA => RecordType::CAA,
            HickoryRecordType::CDS => RecordType::CDS,
            HickoryRecordType::CDNSKEY => RecordType::CDNSKEY,
            HickoryRecordType::CNAME => RecordType::CNAME,
            HickoryRecordType::CSYNC => RecordType::CSYNC,
            HickoryRecordType::DNSKEY => RecordType::DNSKEY,
            HickoryRecordType::DS => RecordType::DS,
            HickoryRecordType::HINFO => RecordType::HINFO,
            HickoryRecordType::HTTPS => RecordType::HTTPS,
            HickoryRecordType::IXFR => RecordType::IXFR,
            HickoryRecordType::KEY => RecordType::KEY,
            HickoryRecordType::MX => RecordType::MX,
            HickoryRecordType::NAPTR => RecordType::NAPTR,
            HickoryRecordType::NS => RecordType::NS,
            HickoryRecordType::NSEC => RecordType::NSEC,
            HickoryRecordType::NSEC3 => RecordType::NSEC3,
            HickoryRecordType::NSEC3PARAM => RecordType::NSEC3PARAM,
            HickoryRecordType::NULL => RecordType::NULL,
            HickoryRecordType::OPENPGPKEY => RecordType::OPENPGPKEY,
            HickoryRecordType::OPT => RecordType::OPT,
            HickoryRecordType::PTR => RecordType::PTR,
            HickoryRecordType::RRSIG => RecordType::RRSIG,
            HickoryRecordType::SIG => RecordType::SIG,
            HickoryRecordType::SOA => RecordType::SOA,
            HickoryRecordType::SRV => RecordType::SRV,
            HickoryRecordType::SSHFP => RecordType::SSHFP,
            HickoryRecordType::SVCB => RecordType::SVCB,
            HickoryRecordType::TLSA => RecordType::TLSA,
            HickoryRecordType::TSIG => RecordType::TSIG,
            HickoryRecordType::TXT => RecordType::TXT,
            HickoryRecordType::Unknown(_code) => RecordType::Unknown,
            HickoryRecordType::ZERO => RecordType::ZERO,
            _ => panic!("Unsupported HickoryRecordType: {:?}", value),
        }
    }
}

impl From<RecordType> for HickoryRecordType {
    fn from(value: RecordType) -> Self {
        match value {
            RecordType::A => HickoryRecordType::A,
            RecordType::AAAA => HickoryRecordType::AAAA,
            RecordType::CNAME => HickoryRecordType::CNAME,
            RecordType::MX => HickoryRecordType::MX,
            RecordType::NS => HickoryRecordType::NS,
            RecordType::SOA => HickoryRecordType::SOA,
            RecordType::TXT => HickoryRecordType::TXT,
            RecordType::CAA => HickoryRecordType::CAA,
            RecordType::SRV => HickoryRecordType::SRV,
            RecordType::PTR => HickoryRecordType::PTR,
            RecordType::SVCB => HickoryRecordType::SVCB,
            RecordType::HTTPS => HickoryRecordType::HTTPS,
            RecordType::ANAME => HickoryRecordType::ANAME,
            RecordType::ANY => HickoryRecordType::ANY,
            RecordType::AXFR => HickoryRecordType::AXFR,
            RecordType::CDS => HickoryRecordType::CDS,
            RecordType::CDNSKEY => HickoryRecordType::CDNSKEY,
            RecordType::CERT => HickoryRecordType::Unknown(0x000A), // CERT
            RecordType::DNAME => HickoryRecordType::Unknown(0x0027), // DNAME
            RecordType::CSYNC => HickoryRecordType::CSYNC,
            RecordType::DNSKEY => HickoryRecordType::DNSKEY,
            RecordType::DS => HickoryRecordType::DS,
            RecordType::HINFO => HickoryRecordType::HINFO,
            RecordType::IXFR => HickoryRecordType::IXFR,
            RecordType::KEY => HickoryRecordType::KEY,
            RecordType::NAPTR => HickoryRecordType::NAPTR,
            RecordType::NSEC => HickoryRecordType::NSEC,
            RecordType::NSEC3 => HickoryRecordType::NSEC3,
            RecordType::NSEC3PARAM => HickoryRecordType::NSEC3PARAM,
            RecordType::NULL => HickoryRecordType::NULL,
            RecordType::OPENPGPKEY => HickoryRecordType::OPENPGPKEY,
            RecordType::OPT => HickoryRecordType::OPT,
            RecordType::RRSIG => HickoryRecordType::RRSIG,
            RecordType::SIG => HickoryRecordType::SIG,
            RecordType::SMIMEA => HickoryRecordType::Unknown(0x0041), // SMIMEA
            RecordType::SSHFP => HickoryRecordType::SSHFP,
            RecordType::TLSA => HickoryRecordType::TLSA,
            RecordType::TSIG => HickoryRecordType::TSIG,
            RecordType::Unknown => HickoryRecordType::Unknown(0xFFFF), // Use a placeholder for unknown types
            RecordType::ZERO => HickoryRecordType::ZERO,
        }
    }
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
#[unique(domain_id, record_type, value)]
pub struct Record {
    #[key]
    #[auto]
    pub id: u64,

    #[index]
    pub domain_id: String,
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

    pub record_type: RecordType,
    pub value: String,
    pub ttl: u32,
}

impl From<RecordDTO> for Record {
    fn from(dto: RecordDTO) -> Self {
        Record {
            id: 0, // This will be auto-generated
            domain_id: dto.domain_id,

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
    pub outbound: Deferred<Vec<Outbound>>,
    #[has_many]
    pub inbound: Deferred<Vec<Inbound>>,
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
pub struct Message {
    #[key]
    #[auto]
    pub id: u64,
    pub mail_from: String,
    pub raw: Vec<u8>,
    pub subject: Option<String>,

    pub message_id_header: Option<String>,
    pub content_type: Option<String>,
    pub from_address: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    #[auto]
    pub created_at: jiff::Timestamp,
    #[auto]
    pub updated_at: jiff::Timestamp,
}

#[derive(Model)]
pub struct Inbound {
    #[key]
    #[auto]
    pub id: u64,

    #[index]
    pub message_id: u64,

    #[index]
    pub rcpt_to: String,

    #[auto]
    pub created_at: jiff::Timestamp,
    #[auto]
    pub updated_at: jiff::Timestamp,

    #[belongs_to(key = rcpt_to, references = id)]
    pub mailbox: Deferred<Mailbox>,

    #[belongs_to(key = message_id, references = id)]
    pub message: Deferred<Message>,
}

#[derive(Model)]
pub struct Outbound {
    #[key]
    #[auto]
    pub id: u64,

    #[index]
    pub message_id: u64,

    pub rcpt_to: String,

    #[index]
    pub sender_id: String,

    #[auto]
    pub created_at: jiff::Timestamp,
    #[auto]
    pub updated_at: jiff::Timestamp,

    #[belongs_to(key = sender_id, references = id)]
    pub mailbox: Deferred<Mailbox>,

    #[belongs_to(key = message_id, references = id)]
    pub message: Deferred<Message>,

    #[has_many]
    pub delivery_statuses: Deferred<Vec<DeliveryStatus>>,
}

#[derive(Model)]
pub struct DeliveryStatus {
    #[key]
    #[auto]
    pub id: u64,

    #[index]
    pub outbound_id: u64,

    pub peer_host: String,
    pub code: Option<u64>,
    pub message: Option<String>,

    #[auto]
    pub created_at: jiff::Timestamp,
    #[auto]
    pub updated_at: jiff::Timestamp,

    #[belongs_to(key = outbound_id, references = id )]
    pub outbound: Deferred<Outbound>,
}
