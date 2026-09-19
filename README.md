# Zonemail

Zonemail is a lightweight, custom-built email service and DNS service written in Rust. It combines a Web/API framework, an asynchronous DNS server, and an SMTP inbound/outbound pipeline into a single self-contained application.

## Features

- **API-Driven Provisioning:** Dynamically provision email addresses and manage routing configurations.
- **Inbound SMTP Server:** Catch and parse incoming emails via `samotop` and `mail-parser`.
- **Outbound SMTP Client:** Dispatch emails seamlessly using `lettre`.
- **Custom DNS Resolution:** Built using `hickory-proto` and `tokio` for handling DNS lookups and records.

## Project Structure

- `src/main.rs`: Entry point and concurrency runtime (`tokio::select!`) managing the API server, DNS server, and Mail listener concurrently.

## Getting Started

1. Clone the repository and navigate into the project directory.
2. Build and run the project using Cargo:
   ```bash
   cargo run
   ```

## Requirements

- Rust (Latest stable edition)
- Cargo