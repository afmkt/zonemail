use crate::app::AppState;
use crate::db::{Domain, Record, RecordType};
use hickory_proto::op::{Message, ResponseCode};
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

    // Create response using Message::response helper
    let mut response = Message::response(request.id, request.op_code);
    response.metadata.recursion_desired = request.recursion_desired;
    response.metadata.recursion_available = false;
    response.metadata.authoritative = true;

    let mut db = app_state.db.clone();

    for query in &request.queries {
        let name = query.name().to_string();
        let clean_name = name.trim_end_matches('.').to_string();
        let qtype = query.query_type();

        info!("DNS Query received for: {} type {:?}", clean_name, qtype);

        response.add_query(query.clone());

        let records = query_records(&mut db, &clean_name, qtype).await;

        if records.is_empty() {
            response.metadata.response_code = ResponseCode::NXDomain;
        } else {
            response.metadata.response_code = ResponseCode::NoError;
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

// Given a query like "mail.sub.zonemail.net" and your known domains from the DB
fn parse_domain_and_name(query_name: &str, known_domains: &[String]) -> Option<(String, String)> {
    let labels: Vec<&str> = query_name.split('.').collect();

    // Try matching from the full string down to smaller suffixes
    for i in 0..labels.len() {
        let candidate_domain = labels[i..].join(".");
        if known_domains.contains(&candidate_domain) {
            let record_name = if i == 0 {
                "@".to_string() // or "" depending on how you store root records
            } else {
                labels[..i].join(".")
            };
            return Some((candidate_domain, record_name));
        }
    }
    None // Domain not hosted by this server
}

async fn query_records(db: &mut toasty::Db, name: &str, qtype: HickoryRecordType) -> Vec<Record> {
    let all_domain = Domain::all().exec(db).await.unwrap_or_else(|_| vec![]);
    if let Some((domain_id, record_name)) = parse_domain_and_name(
        name,
        &all_domain.iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
    ) {
        info!(
            "Matched domain: {}, record name: {}",
            domain_id, record_name
        );
        let all_records = Record::filter(
            Record::fields()
                .name()
                .eq(record_name)
                .and(Record::fields().record_type().eq(RecordType::from(qtype)))
                .and(Record::fields().domain_id().eq(domain_id)),
        )
        .exec(db)
        .await
        .unwrap_or_else(|_| vec![]);
        all_records.into_iter().collect()
    } else {
        info!("No matching domain found for query: {}", name);
        vec![]
    }
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
