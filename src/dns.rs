use crate::app::AppState;
use crate::db::Record;
use hickory_proto::op::{Message, ResponseCode};
use hickory_proto::rr::{DNSClass, RData, RecordType as HickoryRecordType};
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{info, warn};

pub async fn run_dns_server(
    bind_addr: SocketAddr,
    app_ctx: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting real DNS server on {}", bind_addr);
    let socket = UdpSocket::bind(&bind_addr).await?;
    let mut buf = vec![0u8; 512]; // Standard DNS UDP packet size limit

    loop {
        let (len, peer_addr) = match socket.recv_from(&mut buf).await {
            Ok(val) => val,
            Found => continue,
            Err(e) => {
                warn!("Failed to receive UDP packet: {}", e);
                continue;
            }
        };

        let req_bytes = buf[..len].to_vec();
        let ctx = app_ctx.clone();
        let socket_ref = socket.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_dns_query(&socket_ref, peer_addr, &req_bytes, ctx).await {
                warn!("Error handling DNS query from {}: {}", peer_addr, e);
            }
        });
    }
}

async fn handle_dns_query(
    socket: &UdpSocket,
    peer_addr: std::net::SocketAddr,
    req_bytes: &[u8],
    app_ctx: Arc<AppState>,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Parse incoming DNS request message using hickory-proto
    let request = match Message::from_vec(req_bytes) {
        Ok(msg) => msg,
        Err(e) => {
            warn!("Failed to parse incoming DNS packet: {}", e);
            return Ok(());
        }
    };

    let mut response = Message::new();
    response.set_id(request.id());
    response.set_message_type(hickory_proto::op::MessageType::Response);
    response.set_recursion_desired(request.recursion_desired());
    response.set_recursion_available(false);
    response.set_authoritative(true);

    let mut db = app_ctx.db.clone();

    // 2. Process each query in the request
    for query in request.queries() {
        let name = query.name().to_string();
        // Remove trailing dot if present for database matching (e.g., "example.com.")
        let clean_name = name.trim_end_matches('.').to_string();
        let qtype = query.query_type();

        info!("DNS Query received for: {} type {:?}", clean_name, qtype);

        response.add_query(query.clone());

        // 3. Query Toasty database for matching DNS records
        // For simplicity, we search records where name matches `clean_name`
        // (You can refine this logic to parse domain_id or subdomain matching)
        let records = sqlx_or_toasty_query_records(&mut db, &clean_name, qtype).await;

        if records.is_empty() {
            response.set_response_code(ResponseCode::NXDomain);
        } else {
            response.set_response_code(ResponseCode::NoError);
            for rec in records {
                if let Some(rdata) = parse_rdata(qtype, &rec.value) {
                    let mut dns_record = hickory_proto::rr::Record::new();
                    dns_record.set_name(query.name().clone());
                    dns_record.set_dns_class(DNSClass::IN);
                    dns_record.set_ttl(rec.ttl);
                    dns_record.set_data(Some(rdata));
                    response.add_answer(dns_record);
                }
            }
        }
    }

    // 4. Serialize and send back the DNS response
    let res_bytes = response.to_vec()?;
    socket.send_to(&res_bytes, peer_addr).await?;

    Ok(())
}

async fn sqlx_or_toasty_query_records(
    db: &mut toasty::Db,
    name: &str,
    qtype: HickoryRecordType,
) -> Vec<Record> {
    // Query your Toasty database for records matching the name and record type string
    let type_str = match qtype {
        HickoryRecordType::A => "A",
        HickoryRecordType::AAAA => "AAAA",
        HickoryRecordType::CNAME => "CNAME",
        HickoryRecordType::MX => "MX",
        HickoryRecordType::TXT => "TXT",
        HickoryRecordType::NS => "NS",
        _ => return vec![],
    };

    // Using Toasty queries or fallback query pattern
    // In actual code, use your Toasty finder or query builder
    let all_records = Record::query().all(db).await.unwrap_or_default();

    all_records
        .into_iter()
        .filter(|r| r.name == name && r.record_type == type_str)
        .collect()
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
            // Format: "priority preference" e.g., "10 mail.zonemail.net"
            let parts: Vec<&str> = value.split_whitespace().collect();
            if parts.len() == 2 {
                let pref = parts[0].parse().unwrap_or(10);
                let mx_name = hickory_proto::rr::Name::from_str(parts[1]).ok()?;
                Some(RData::MX(hickory_proto::rr::rdata::MX::new(pref, mx_name)))
            } else {
                None
            }
        }
        _ => None,
    }
}
