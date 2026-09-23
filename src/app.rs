use crate::config::Config;
use crate::db::{Domain, Mailbox, Record};
// use std::path::Path;
// use tokio::fs::*;
use tracing::info;

// async fn ensure_dir(dir: &Path) -> Result<(), std::io::Error> {
//     let exists = try_exists(dir).await?;
//     if !exists {
//         create_dir_all(dir).await?;
//     }
//     if metadata(dir).await?.is_dir() {
//         Ok(())
//     } else {
//         Err(std::io::Error::new(
//             std::io::ErrorKind::InvalidInput,
//             format!("{} is not a directory", dir.display()),
//         ))
//     }
// }

#[derive(Clone)]
pub struct AppState {
    pub db: toasty::Db,
}

impl AppState {
    pub async fn init(config: &Config) -> Result<Self, Box<dyn std::error::Error>> {
        let driver = toasty_driver_turso::Turso::file(&config.database_url).concurrent_writes();

        let db = toasty::Db::builder()
            .models(toasty::models!(crate::*))
            .build(driver)
            .await?;

        // 2. Automatically push/ensure schema tables are created in the database
        // (Toasty connections support push_schema to set up tables and indices based on models)
        // Wrap in a loop that tolerates 'table already exists' from a previous run;
        // push_schema emits CREATE TABLE without IF NOT EXISTS.
        match db.push_schema().await {
            Ok(()) => info!("Database schema synchronized successfully."),
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("already exists") {
                    info!("Database schema already up to date, skipping.");
                } else {
                    return Err(Box::new(e));
                }
            }
        }

        let mut ctx = Self { db };

        // 3. Seed data from config
        ctx.seed_from_config(config).await?;

        Ok(ctx)
    }
    async fn seed_from_config(
        &mut self,
        config: &Config,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Seed Domains
        for domain_id in &config.domains {
            // Check if domain already exists, or upsert/create it idempotently
            let domain_exists = Domain::get_by_id(&mut self.db, domain_id).await.is_ok();
            if !domain_exists {
                let domain: Domain = domain_id.clone().into();
                toasty::create!(Domain { id: domain.id })
                    .exec(&mut self.db)
                    .await?;
                info!("Seeded domain: {}", domain_id);
            }
        }

          // Seed Mailboxes
        for entry in &config.mailboxes {
            if let Ok(mut mailbox) = Mailbox::try_from(entry.address()) {
                   // Normalize to lowercase so the open-relay check in `handle_rcpt`
                   // (which lowercases the incoming address) matches provisioned mailboxes
                   // regardless of the case used in config.
                 mailbox.id = mailbox.id.to_lowercase();
                 let exists = Mailbox::get_by_id(&mut self.db, &mailbox.id).await.is_ok();
                 if !exists {
                     toasty::create!(Mailbox {
                         id: mailbox.id,
                         domain_id: mailbox.domain_id,
                         forward_to: entry.forward_to().map(String::from),
                       })
                       .exec(&mut self.db)
                       .await?;
                     info!(
                          "Seeded mailbox: {} (forward_to: {:?})",
                        entry.address(), entry.forward_to()
                       );
                   }
               }
           }
        // Seed DNS Records
        for record_dto in &config.records {
            let record: Record = record_dto.into();
            // Insert DNS records
            toasty::create!(Record {
                domain_id: record.domain_id,

                record_type: record.record_type,
                value: record.value,
                ttl: record.ttl,
            })
            .exec(&mut self.db)
            .await?;
            info!(
                "Seeded DNS record [{:?}] -> {}",
                record_dto.record_type, record_dto.value
            );
        }

        Ok(())
    }
}
