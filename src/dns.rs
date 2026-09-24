use crate::app::AppState;
use crate::db::{Record, RecordType};
use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::rr::{Name, RData, RecordType as HickoryRecordType};
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{info, warn};

pub async fn run_dns_server(
    bind_addr: SocketAddr,
    app_state: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting real DNS server on {}", bind_addr);
    let socket = Arc::new(UdpSocket::bind(bind_addr).await?);
    let mut buf = vec![0u8; 512];

    loop {
        let (len, peer_addr) = match socket.recv_from(&mut buf).await {
            Ok(val) => val,
            Err(e) => {
                warn!("Failed to receive UDP packet: {}", e);
                continue;
            }
        };

        let req_bytes = buf[..len].to_vec();
        let state = app_state.clone();
        let socket_ref = socket.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_dns_query(&socket_ref, peer_addr, &req_bytes, state).await {
                warn!("Error handling DNS query from {}: {}", peer_addr, e);
            }
        });
    }
}

async fn handle_dns_query(
    socket: &UdpSocket,
    peer_addr: SocketAddr,
    req_bytes: &[u8],
    app_state: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = match Message::from_vec(req_bytes) {
        Ok(msg) => msg,
        Err(e) => {
            warn!("Failed to parse incoming DNS packet: {}", e);
            return Ok(());
        }
    };

    // Create response using the builder-style API available in hickory-proto 0.24
    let mut response = Message::new();
    response.set_message_type(MessageType::Response);
    response.set_id(request.id());
    response.set_op_code(request.header().op_code());
    response.set_recursion_desired(request.header().recursion_desired());
    response.set_recursion_available(false);
    response.set_authoritative(true);

    let mut db = app_state.db.clone();

    for query in request.queries() {
        let name = query.name().to_string();
        let clean_name = name.trim_end_matches('.').to_string();
        let qtype = query.query_type();

        info!("DNS Query received for: {} type {:?}", clean_name, qtype);

        response.add_query(query.clone());

        let records = query_records(&mut db, &clean_name, qtype).await;

        if records.is_empty() {
            response.set_response_code(ResponseCode::NXDomain);
        } else {
            response.set_response_code(ResponseCode::NoError);
            for rec in records {
                if let Some(rdata) = parse_rdata(qtype, &rec.value) {
                    let dns_record =
                        hickory_proto::rr::Record::from_rdata(query.name().clone(), rec.ttl, rdata);
                    response.add_answer(dns_record);
                }
            }
        }
    }
    let res_bytes = response.to_vec()?;
    socket.send_to(&res_bytes, peer_addr).await?;

    Ok(())
}

async fn query_records(db: &mut toasty::Db, name: &str, qtype: HickoryRecordType) -> Vec<Record> {
    let all_records = Record::filter(
        Record::fields()
            .domain_id()
            .eq(name)
            .and(Record::fields().record_type().eq(RecordType::from(qtype))),
    )
    .exec(db)
    .await
    .unwrap_or_else(|_| vec![]);
    all_records.into_iter().collect()
}

fn parse_rdata(qtype: HickoryRecordType, value: &str) -> Option<RData> {
    match qtype {
        HickoryRecordType::A => {
            let ipv4 = Ipv4Addr::from_str(value).ok()?;
            Some(RData::A(hickory_proto::rr::rdata::A(ipv4)))
        }
        HickoryRecordType::TXT => {
            let txt = hickory_proto::rr::rdata::TXT::new(vec![value.to_string()]);
            Some(RData::TXT(txt))
        }
        HickoryRecordType::MX => {
            let parts: Vec<&str> = value.split_whitespace().collect();
            if parts.len() == 2 {
                let pref = parts[0].parse().unwrap_or(10);
                let mx_name = Name::from_str(parts[1]).ok()?;
                Some(RData::MX(hickory_proto::rr::rdata::MX::new(pref, mx_name)))
            } else {
                None
            }
        }
        HickoryRecordType::PTR => {
            let ptr_name = Name::from_str(value).ok()?;
            Some(RData::PTR(hickory_proto::rr::rdata::PTR(ptr_name)))
        }
        _ => None,
    }
}

// ===========================================================================
// Tests
// ===========================================================================
//
// `parse_rdata` is a pure function turning a stored record string into hickory's
// `RData`; these cover every supported type plus the unsupported/invalid arms.
// One integration-style test exercises `query_records` against an in-memory DB.
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

      #[test]
    fn parse_a_record_yields_ipv4() {
        let rdata = parse_rdata(HickoryRecordType::A, "93.184.216.34").expect("A record");
        match rdata {
             RData::A(a) => assert_eq!(a.0, Ipv4Addr::new(93, 184, 216, 34)),
             other => panic!("expected A, got {:?}", other),
          }
      }

      #[test]
    fn parse_txt_record_roundtrips_text() {
        let rdata =
             parse_rdata(HickoryRecordType::TXT, "v=spf1 include:_simplelogin.com ~all").expect("TXT");
        match rdata {
             RData::TXT(txt) => {
                let texts: Vec<String> = txt
                     .txt_data()
                     .iter()
                     .map(|chunk| String::from_utf8_lossy(&chunk[..]).to_string())
                     .collect();
                assert_eq!(texts, vec!["v=spf1 include:_simplelogin.com ~all"]);
              }
             other => panic!("expected TXT, got {:?}", other),
          }
      }

      #[test]
    fn parse_mx_record_keeps_preference_and_exchange() {
        let rdata = parse_rdata(HickoryRecordType::MX, "10 mail.example.com").expect("MX");
        match rdata {
             RData::MX(mx) => {
                assert_eq!(mx.preference(), 10);
                let exchange = mx.exchange().to_string();
                     // hickory appends the FQDN trailing dot when displaying a Name.
                assert!(exchange.starts_with("mail.example.com"), "exchange={exchange}");
              }
             other => panic!("expected MX, got {:?}", other),
          }
      }

      #[test]
    fn parse_mx_defaults_preference_malformed_but_keeps_value() {
         // A non-numeric preference falls back to 10; the exchange still parses.
        let rdata = parse_rdata(HickoryRecordType::MX, "pref mail.example.com").expect("MX");
        match rdata {
             RData::MX(mx) => assert_eq!(mx.preference(), 10),
             other => panic!("expected MX, got {:?}", other),
          }
      }

      #[test]
    fn parse_ptr_record_yields_pointer() {
        let rdata = parse_rdata(HickoryRecordType::PTR, "target.example.com").expect("PTR");
        assert!(matches!(rdata, RData::PTR(_)));
      }

      #[test]
    fn parse_rdata_unsupported_type_returns_none() {
          // CAA is a supported DB type but unsupported by the serializer, so it
          // maps to `None` rather than a bogus record.
        assert!(parse_rdata(HickoryRecordType::CAA, "0 \"issue\" \"example.com\"").is_none());
      }

      #[test]
    fn parse_a_rejects_non_ip_value() {
        assert!(parse_rdata(HickoryRecordType::A, "not-an-ip-address").is_none());
      }

      #[test]
    fn parse_mx_rejects_wrong_token_count() {
          // MX needs exactly `preference exchange`; one token or three is rejected.
        assert!(parse_rdata(HickoryRecordType::MX, "onlyone").is_none());
        assert!(parse_rdata(HickoryRecordType::MX, "10 a.example b.example").is_none());
      }

      #[tokio::test]
    async fn query_records_matches_domain_and_type_exactly() {
         let mut db = crate::app::AppState::connect_in_memory().await.unwrap().db;

             // Seed two records for `example.com` (A + MX) and one for `other.com`.
        toasty::create!(Record {
             domain_id: "example.com".to_string(),
             record_type: RecordType::A,
             value: "1.2.3.4".to_string(),
             ttl: 3600,
         }).exec(&mut db).await.expect("insert A");
         toasty::create!(Record {
             domain_id: "example.com".to_string(),
             record_type: RecordType::MX,
             value: "10 mail.example.com".to_string(),
             ttl: 3600,
         }).exec(&mut db).await.expect("insert MX");
         toasty::create!(Record {
             domain_id: "other.com".to_string(),
             record_type: RecordType::A,
             value: "5.6.7.8".to_string(),
             ttl: 3600,
         }).exec(&mut db).await.expect("insert other A");

             // Only the `example.com` A record is returned by an (A) query.
        let a_records = query_records(&mut db, "example.com", HickoryRecordType::A).await;
        assert_eq!(a_records.len(), 1);
        assert_eq!(a_records[0].value, "1.2.3.4");

             // An MX query on the same domain returns the MX record, not the A.
        let mx_records = query_records(&mut db, "example.com", HickoryRecordType::MX).await;
        assert_eq!(mx_records.len(), 1);
        assert_eq!(mx_records[0].value, "10 mail.example.com");

             // A different domain with the same type matches only its own row.
        let other = query_records(&mut db, "other.com", HickoryRecordType::A).await;
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].value, "5.6.7.8");

             // An absent domain yields no records.
        let none = query_records(&mut db, "absent.com", HickoryRecordType::A).await;
        assert!(none.is_empty());
      }
}
