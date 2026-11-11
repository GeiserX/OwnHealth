//! `ownhealth serve | migrate | demo-seed | worker | healthcheck | backup | restore | verify-restore |
//! clinvar-diff`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ownhealth::app;
use ownhealth_core::config::Settings;
use ownhealth_core::{Runtime, demo, jobs, migrate};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(name = "ownhealth", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the web app, MCP server and job loop. Needs OWNHEALTH_DATABASE_URL and OWNHEALTH_SECRET_KEY.
    Serve,
    /// Apply pending database migrations. Needs OWNHEALTH_DATABASE_URL.
    Migrate,
    /// Load the synthetic Rivera family. Needs OWNHEALTH_DATABASE_URL.
    DemoSeed {
        /// Seed even if the demo household exists.
        #[arg(long)]
        force: bool,
    },
    /// Run only the job loop (OWNHEALTH_QUEUES). Needs OWNHEALTH_DATABASE_URL.
    Worker,
    /// Exit 0 when /healthz answers on 127.0.0.1:OWNHEALTH_PORT; print why when it does not.
    Healthcheck,
    /// Write a consistent backup directory (database dump, blobs, manifest) for restic, readable by
    /// its owner only. Needs OWNHEALTH_DATABASE_URL and pg_dump 17.
    Backup {
        /// The directory to write: new, or empty.
        path: PathBuf,
    },
    /// Load a backup directory into this empty instance and check every state_hash. Needs
    /// OWNHEALTH_DATABASE_URL and pg_restore 17.
    Restore {
        /// A directory `ownhealth backup` wrote.
        path: PathBuf,
    },
    /// Restore a backup into a scratch database, compare with its manifest, drop it. Needs
    /// OWNHEALTH_DATABASE_URL and pg_restore 17.
    VerifyRestore {
        /// A directory `ownhealth backup` wrote.
        path: PathBuf,
    },
    /// Compare every current genome with a ClinVar release (the GRCh38 VCF, bgzipped or plain, from a
    /// local path), write the events and alerts, and print what each dataset got. Exits 1 when a
    /// dataset could not be read. Needs OWNHEALTH_DATABASE_URL.
    ClinvarDiff {
        /// The ClinVar VCF, for example clinvar_20261005.vcf.gz.
        path: PathBuf,
    },
}

/// `host:port` of a database URL, for an error a person can act on (never the password).
fn db_target(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    let host = rest.split(['/', '?']).next().unwrap_or_default();
    if host.is_empty() {
        "localhost:5432".into()
    } else if host.contains(':') && !host.ends_with(']') {
        host.into()
    } else {
        format!("{host}:5432")
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // Logs go to stderr, as Python's logging did: stdout carries the reports `verify-restore` and
    // `restore` print, which deploy/restic/restore-test.sh and cron logs read as JSON.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        // colour only on a terminal: container logs and cron mail get plain text
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let cli = Cli::parse();
    match run(cli.cmd).await {
        Ok(code) => code,
        Err(e) => {
            let text = format!("{e:#}");
            if text.contains("pool timed out") || text.contains("Connection refused") {
                // the same source the command used: the variable, its _FILE, or the built-in default
                let url = Settings::from_env().map(|s| s.database_url).unwrap_or_default();
                eprintln!(
                    "error: could not reach the database at {} (set OWNHEALTH_DATABASE_URL, see compose.yaml): {text}",
                    db_target(&url)
                );
            } else {
                eprintln!("error: {text}");
            }
            ExitCode::FAILURE
        }
    }
}

fn report(r: &serde_json::Value) -> anyhow::Result<ExitCode> {
    println!("{}", serde_json::to_string_pretty(r)?);
    Ok(if r["ok"] == serde_json::Value::Bool(true) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn run(cmd: Cmd) -> anyhow::Result<ExitCode> {
    ownhealth::register_lanes();
    let settings = Settings::from_env()?;
    match cmd {
        Cmd::Serve => {
            app::serve(settings).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Backup { path } => {
            println!("{}", ownhealth_ops::backup(&settings, &path).await?.display());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Restore { path } => report(&ownhealth_ops::restore(&settings, &path).await?),
        Cmd::VerifyRestore { path } => report(&ownhealth_ops::verify_restore(&settings, &path).await?),
        Cmd::Healthcheck => Ok(match healthcheck(settings.port).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(why) => {
                eprintln!("unhealthy: http://127.0.0.1:{}/healthz {why}", settings.port);
                ExitCode::FAILURE
            }
        }),
        Cmd::Migrate => {
            let rt = Runtime::new(settings)?;
            let applied = migrate::migrate_embedded(&rt.pool).await?;
            if applied.is_empty() {
                println!("applied: nothing");
            } else {
                println!("applied: {applied:?}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::DemoSeed { force } => {
            let rt = Runtime::new(settings)?;
            migrate::migrate_embedded(&rt.pool).await?;
            if demo::seed_demo(&rt.pool, &rt.settings, &rt.blobs, force).await? {
                println!("demo household seeded");
            } else {
                println!("demo household exists; use --force to add another");
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::ClinvarDiff { path } => {
            let rt = Runtime::new(settings)?;
            let r = ownhealth_genome::clinvar::diff_release(&rt.pool, &rt.blobs, &path, rt.settings.age_of_majority)
                .await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
            // A dataset that could not be read is reported and the others still run; the exit code
            // says so, for a cron or an operator.
            Ok(if r.datasets.iter().any(|d| d.error.is_some()) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Cmd::Worker => {
            let rt = Runtime::new(settings)?;
            app::sweep_spools(&rt);
            // OCR turned on after scans arrived (or a 0.2.0 database): queue the scans that wait
            if rt.settings.ocr && rt.settings.queues.iter().any(|q| q == "ocr") {
                match ownhealth_ocr::job::backfill(&rt.pool).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(documents = n, "queued waiting scans for OCR"),
                    Err(e) => tracing::warn!(error = %e, "could not queue waiting scans for OCR"),
                }
            }
            let (stop, rx) = tokio::sync::watch::channel(false);
            let workers = jobs::start_workers(&rt.job_context(), rx);
            app::shutdown_signal().await;
            let _ = stop.send(true);
            for w in workers {
                let _ = w.await;
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// A plain HTTP/1.0 GET of /healthz on loopback, 3 s budget, like Python's urlopen check. The error
/// is the status line the server answered, or why there was none.
async fn healthcheck(port: u16) -> Result<(), String> {
    let probe = async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        s.write_all(b"GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").await?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    };
    match tokio::time::timeout(std::time::Duration::from_secs(3), probe).await {
        Err(_) => Err("no answer in 3 s".into()),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(buf)) if buf.starts_with(b"HTTP/1.0 200") || buf.starts_with(b"HTTP/1.1 200") => Ok(()),
        Ok(Ok(buf)) => {
            let line = String::from_utf8_lossy(&buf);
            Err(line.lines().next().unwrap_or("an empty answer").to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::db_target;

    #[test]
    fn database_target_names_host_and_port_never_the_password() {
        assert_eq!(
            db_target("postgresql://oh:s3cret@db:5433/oh?sslmode=require"),
            "db:5433"
        );
        assert_eq!(db_target("postgresql+psycopg://oh@db/oh"), "db:5432");
        assert_eq!(db_target("postgresql://127.0.0.1:55432/x"), "127.0.0.1:55432");
    }
}
