mod check;
mod config;
mod db;
mod domain;
mod notify;
mod ratelimit;
mod scheduler;
mod state;

use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::check::{CheckError, Checker, Lookup};
use crate::config::Config;
use crate::db::{Db, DomainRow, HistoryRow, NewDomain};
use crate::notify::Notifier;
use crate::state::{Decision, Event, Observation, Previous, State};

/// DomainGözcü: monitors domain registration status and notifies on changes.
#[derive(Parser)]
#[command(name = "domain-gozcu", version, about)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(
        short,
        long,
        default_value = "/etc/domain-gozcu/config.toml",
        global = true
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the monitoring service (daemon mode, for systemd).
    Run,
    /// Add a domain to the watch list.
    Add {
        domain: String,
        /// Free-form note.
        #[arg(long = "not")]
        note: Option<String>,
        /// Tag (repeatable).
        #[arg(long = "etiket")]
        tags: Vec<String>,
    },
    /// Remove a domain from the watch list.
    Remove { domain: String },
    /// List watched domains.
    List {
        /// Filter by state.
        #[arg(long = "durum")]
        state: Option<String>,
    },
    /// Run a one-off check and print details (no notifications).
    Check { domain: String },
    /// Import domains from a file, one per line.
    Import { file: PathBuf },
    /// Show state change history for a domain.
    History { domain: String },
    /// Send a test message to all enabled channels.
    TestNotify,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run => run(&cli.config).await,
        Command::Check { domain } => check(&cli.config, &domain).await,
        Command::TestNotify => test_notify(&cli.config).await,
        Command::Add { domain, note, tags } => {
            let db = open_db(&cli.config)?;
            let d = domain::normalize(&domain)?;
            let added = db.add(&NewDomain {
                name: &d.ascii,
                display_name: &d.display,
                note: note.as_deref(),
                tags: &tags,
            })?;
            println!(
                "{}: {}",
                d.display,
                if added { "eklendi" } else { "zaten listede" }
            );
            Ok(())
        }
        Command::Remove { domain } => {
            let db = open_db(&cli.config)?;
            let d = domain::normalize(&domain)?;
            let removed = db.remove(&d.ascii)?;
            println!(
                "{}: {}",
                d.display,
                if removed { "silindi" } else { "listede yok" }
            );
            Ok(())
        }
        Command::List { state } => {
            let state = state.map(|s| s.parse::<State>()).transpose()?;
            let db = open_db(&cli.config)?;
            print_list(&db.list(state.map(State::as_str))?);
            Ok(())
        }
        Command::Import { file } => {
            let db = open_db(&cli.config)?;
            import(&db, &file)
        }
        Command::History { domain } => {
            let db = open_db(&cli.config)?;
            let d = domain::normalize(&domain)?;
            let Some(row) = db.get(&d.ascii)? else {
                anyhow::bail!("{}: listede yok", d.display);
            };
            print_history(&row.display_name, &db.history(row.id)?);
            Ok(())
        }
    }
}

/// Sends a test message straight to every enabled channel (bypassing the queue).
async fn test_notify(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    config.validate_channels()?;
    init_logging(&config);
    let channels = notify::enabled_channels(&config);
    if channels.is_empty() {
        anyhow::bail!("etkin bildirim kanalı yok");
    }
    let notifier = Notifier::new(&config)?;
    let message = notify::test_message();
    let mut failed = 0;
    for channel in channels {
        match notifier.send(channel, &message).await {
            Ok(status) => println!("✅ {channel}: gönderildi (HTTP {status})"),
            Err(err) => {
                failed += 1;
                println!("❌ {channel}: {err}");
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} kanala gönderilemedi");
    }
    Ok(())
}

async fn run(config_path: &Path) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    config.validate_channels()?;
    init_logging(&config);
    let db_path = PathBuf::from(&config.general.database);
    scheduler::run(config, &db_path).await
}

fn init_logging(config: &Config) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.general.log_level));
    // Ignore the error: a subscriber may already be installed.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Loads the config, initialises logging, opens the database and syncs `[[domain]]` entries.
fn open_db(config_path: &Path) -> anyhow::Result<Db> {
    let config = Config::load(config_path)?;
    init_logging(&config);
    let db = Db::open(Path::new(&config.general.database))?;
    db.sync_config_domains(&config.domains)?;
    Ok(db)
}

/// One-off check: prints the raw report and the decision the state machine would take.
/// Touches neither the database (read-only) nor notifiers.
async fn check(config_path: &Path, input: &str) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    init_logging(&config);
    let d = domain::normalize(input)?;
    let checker = Checker::new(&config)?;
    let now = Utc::now();
    let observation = checker.observe(&d.ascii, now).await;

    println!("Domain:         {}", d.display);
    print_observation(&observation);

    let (previous, listed) = load_previous(&config, &d.ascii)?;
    let decision = state::decide(&d.ascii, &previous, &observation, now, &config);
    println!();
    println!(
        "Önceki durum:   {} ({})",
        previous.state,
        if listed {
            "listede"
        } else {
            "listede değil, ilk kontrol varsayıldı"
        }
    );
    print_decision(&decision, now);
    Ok(())
}

/// Reads the stored snapshot without creating or modifying the database.
fn load_previous(config: &Config, name: &str) -> anyhow::Result<(Previous, bool)> {
    let path = Path::new(&config.general.database);
    if !path.exists() {
        return Ok((Previous::default(), false));
    }
    let stored = Db::open(path)?.previous(name)?;
    let listed = stored.is_some();
    Ok((stored.unwrap_or_default(), listed))
}

fn print_observation(observation: &Observation) {
    match observation {
        Observation::Err(CheckError::Transient(msg)) => {
            println!("Sonuç:          geçici hata ({msg})")
        }
        Observation::Err(CheckError::Permanent(msg)) => println!("Sonuç:          hata ({msg})"),
        Observation::Ok {
            source,
            lookup,
            dns_ns,
            parking,
        } => {
            println!("NS (DNS):       {}", join_or_dash(dns_ns));
            println!("Kaynak:         {source}");
            match lookup {
                Lookup::NotFound => println!("Sonuç:          bulunamadı (kayıt yok)"),
                Lookup::Registered(reg) => {
                    println!("Sonuç:          kayıtlı");
                    println!("Durum:          {}", join_or_dash(&reg.statuses));
                    println!(
                        "Registrar:      {}",
                        reg.registrar.as_deref().unwrap_or("-")
                    );
                    println!("NS (kayıt):     {}", join_or_dash(&reg.nameservers));
                    println!("Kayıt tarihi:   {}", full_time(reg.registered));
                    println!("Bitiş tarihi:   {}", full_time(reg.expires));
                    println!("Son değişiklik: {}", full_time(reg.last_changed));
                    println!(
                        "Park/satılık:   {}",
                        parking.as_ref().map_or("iz yok", |p| p.reason.as_str())
                    );
                }
            }
        }
    }
}

fn print_decision(decision: &Decision, now: DateTime<Utc>) {
    println!("Karar:          {}", decision.state);
    println!(
        "Sonraki kontrol: {} sonra ({})",
        format_duration(decision.next_check_in),
        full_time(Some(now + decision.next_check_in))
    );
    println!("Hata sayacı:    {}", decision.error_count);
    println!(
        "Boşta doğrulama: {}",
        if decision.pending_free_confirm {
            "ikinci teyit bekleniyor"
        } else {
            "yok"
        }
    );
    println!(
        "PendingDelete:  {}",
        full_time(decision.pending_delete_since)
    );
    println!(
        "Bildirilen eşikler: {}",
        if decision.notified_thresholds.is_empty() {
            "-".to_string()
        } else {
            decision
                .notified_thresholds
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    match &decision.history {
        Some(h) => println!("Geçmiş kaydı:   {} -> {} ({})", h.from, h.to, h.detail),
        None => println!("Geçmiş kaydı:   yok"),
    }
    println!(
        "Kayıt verisi:   {}",
        if decision.registration.is_some() {
            "güncellenir"
        } else {
            "korunur"
        }
    );
    if decision.events.is_empty() {
        println!("Olaylar (bildirim gönderilmez): yok");
    } else {
        println!("Olaylar (bildirim gönderilmez):");
        for event in &decision.events {
            println!("  - {}", describe_event(event));
        }
    }
}

fn describe_event(event: &Event) -> String {
    match event {
        Event::StateChanged { from, to, detail } => {
            format!("durum değişti: {from} -> {to} ({detail})")
        }
        Event::Retaken { registrar } => {
            format!(
                "tekrar alındı (registrar: {})",
                registrar.as_deref().unwrap_or("-")
            )
        }
        Event::PendingDelete { estimated_drop } => {
            format!(
                "pendingDelete, tahmini düşme: {}",
                full_time(Some(*estimated_drop))
            )
        }
        Event::BecameFree => "BOŞTA (2 kez doğrulandı)".into(),
        Event::ExpiryWarning { days_left } => format!("bitişe {days_left} gün kaldı"),
        Event::Renewed { new_expires } => {
            format!("yenilendi, yeni bitiş: {}", full_time(Some(*new_expires)))
        }
        Event::Unreachable {
            error_count,
            reason,
        } => {
            format!("kontrol edilemiyor ({error_count} ardışık hata): {reason}")
        }
    }
}

fn format_duration(d: Duration) -> String {
    let minutes = d.num_minutes();
    if minutes > 0 && minutes % 1440 == 0 {
        format!("{} gün", minutes / 1440)
    } else if minutes > 0 && minutes % 60 == 0 {
        format!("{} saat", minutes / 60)
    } else {
        format!("{minutes} dk")
    }
}

fn join_or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "-".into()
    } else {
        items.join(", ")
    }
}

fn full_time(time: Option<DateTime<Utc>>) -> String {
    time.map_or("-".into(), |t| t.format("%Y-%m-%d %H:%M UTC").to_string())
}

fn import(db: &Db, file: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("dosya okunamadı: {}", file.display()))?;
    let (mut added, mut skipped, mut invalid) = (0, 0, 0);
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match domain::normalize(line) {
            Ok(d) => {
                let new = NewDomain {
                    name: &d.ascii,
                    display_name: &d.display,
                    note: None,
                    tags: &[],
                };
                if db.add(&new)? {
                    added += 1;
                } else {
                    skipped += 1;
                }
            }
            Err(err) => {
                invalid += 1;
                eprintln!("satır {}: geçersiz ({line}): {err}", index + 1);
            }
        }
    }
    println!("eklenen: {added}, zaten listede: {skipped}, geçersiz: {invalid}");
    Ok(())
}

/// Formats an RFC3339 timestamp as `YYYY-MM-DD HH:MM` (UTC), falling back to the raw text.
fn short_time(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|t| t.with_timezone(&Utc).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| ts.to_string())
}

fn print_table(header: &[&str], rows: &[Vec<String>]) {
    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain([header[i].chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: &[String]| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        println!("{}", padded.join("  ").trim_end());
    };
    line(&header.iter().map(|h| h.to_string()).collect::<Vec<_>>());
    for row in rows {
        line(row);
    }
}

fn print_list(domains: &[DomainRow]) {
    if domains.is_empty() {
        println!("Kayıtlı domain yok.");
        return;
    }
    let rows: Vec<Vec<String>> = domains
        .iter()
        .map(|d| {
            vec![
                d.display_name.clone(),
                d.state.clone(),
                d.expires_at.as_deref().map_or("-".into(), short_time),
                short_time(&d.next_check_at),
            ]
        })
        .collect();
    print_table(&["DOMAIN", "DURUM", "BİTİŞ", "SONRAKİ KONTROL"], &rows);
}

fn print_history(display_name: &str, history: &[HistoryRow]) {
    if history.is_empty() {
        println!("{display_name}: durum geçmişi yok.");
        return;
    }
    let rows: Vec<Vec<String>> = history
        .iter()
        .map(|h| {
            vec![
                short_time(&h.at),
                h.from_state.clone().unwrap_or_else(|| "-".into()),
                h.to_state.clone(),
                h.detail.clone().unwrap_or_default(),
            ]
        })
        .collect();
    print_table(&["ZAMAN", "ESKİ", "YENİ", "AYRINTI"], &rows);
}
