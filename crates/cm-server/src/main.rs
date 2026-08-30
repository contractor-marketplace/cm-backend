//! The service binary.
//!
//! One executable with subcommands rather than several binaries: on a single
//! VPS that means one artefact to build, ship and roll back, and one systemd
//! unit per role invoking the same file with different arguments.

use clap::{Parser, Subcommand};
use cm_core::Config;
use std::process::ExitCode;
use std::time::Duration;

/// Exit code for a configuration problem, distinct from a runtime failure so a
/// deploy script can tell "the env file is wrong" from "the database is down".
const EXIT_CONFIG: u8 = 2;
const EXIT_RUNTIME: u8 = 1;

#[derive(Debug, Parser)]
#[command(name = "cm-server", about = "Contractor marketplace backend", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the HTTP API.
    Serve,
    /// Apply outstanding database migrations, then exit.
    Migrate,
    /// Validate the environment and print the resolved configuration.
    ///
    /// Deploy preflight: proves an environment file parses before the unit is
    /// restarted against it.
    CheckConfig,
    /// Operator actions with no HTTP surface.
    #[command(subcommand)]
    Admin(AdminCommand),
    /// Import an operator-supplied CSLB licence file.
    ImportCslb {
        /// Path to the file downloaded from the CSLB Public Data Portal.
        #[arg(long)]
        file: std::path::PathBuf,
        /// cslb_master_list | cslb_county_list
        #[arg(long, default_value = "cslb_master_list")]
        source: String,
        /// Restrict to one county, e.g. "LOS ANGELES".
        #[arg(long)]
        county: Option<String>,
        /// CSLB's own "current as of" date, YYYY-MM-DD.
        #[arg(long)]
        snapshot_date: Option<String>,
        #[arg(long, default_value_t = cm_domain::import::DEFAULT_BATCH)]
        batch: usize,
        /// Parse and count without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Load ZIP-code centroids, which are the published location for every
    /// contractor whose exact address is protected.
    LoadRegions {
        /// CSV with columns: code, name, lat, lon.
        #[arg(long)]
        file: std::path::PathBuf,
        #[arg(long, default_value = "operator_supplied")]
        source: String,
    },
    /// Insert the canonical trade set. Idempotent.
    SeedTrades,
    /// Re-derive the verified badge for every contractor.
    RecomputeVerification,
    /// Delete what nothing needs any more, in bounded batches.
    Prune {
        /// Days to keep a finished session or geocode job.
        #[arg(long, default_value_t = cm_domain::maintenance::DEFAULT_GRACE_DAYS)]
        grace_days: i64,
        /// Days of audit log to keep. Omitted means keep everything: deleting
        /// an audit trail is a policy decision, not housekeeping.
        #[arg(long)]
        audit_days: Option<i64>,
        /// Print row counts and exit without deleting anything.
        #[arg(long)]
        report_only: bool,
    },
    /// Resolve queued addresses into coordinates.
    GeocodeWorker {
        /// Run one pass and exit, instead of looping.
        #[arg(long)]
        once: bool,
        #[arg(long, default_value = "us_census")]
        provider: String,
        /// Override the provider endpoint, for a staging or recorded service.
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long, default_value_t = 25)]
        batch: i64,
        /// Seconds to wait when a pass finds nothing.
        #[arg(long, default_value_t = 30)]
        idle_secs: u64,
    },
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// Grant a role to an account.
    ///
    /// There is deliberately no HTTP endpoint for this. The first admin has to
    /// come from somewhere, and an endpoint that can create admins is an
    /// endpoint worth attacking; shell access to the box is a stronger
    /// prerequisite than any check we could write.
    GrantRole {
        #[arg(long)]
        email: String,
        #[arg(long)]
        role: String,
    },
    /// Remove a role from an account.
    RevokeRole {
        #[arg(long)]
        email: String,
        #[arg(long)]
        role: String,
    },
    /// Show an account's roles.
    ShowRoles {
        #[arg(long)]
        email: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Configuration is resolved before the runtime starts, so a bad environment
    // fails in milliseconds with a readable message rather than at the first
    // request that happens to touch the bad value.
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(errors) => {
            eprintln!("{errors}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    if let Command::CheckConfig = cli.command {
        for (key, value) in config.redacted_summary() {
            println!("{key} = {value}");
        }
        return ExitCode::SUCCESS;
    }

    if let Err(error) = cm_core::telemetry::init(config.environment, config.log_format) {
        eprintln!("could not install the tracing subscriber: {error}");
        return ExitCode::from(EXIT_RUNTIME);
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start the async runtime: {error}");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };

    let result = runtime.block_on(async {
        match cli.command {
            Command::Serve => serve(config).await,
            Command::Migrate => migrate(config).await,
            Command::Admin(command) => admin(config, command).await,
            Command::ImportCslb {
                file,
                source,
                county,
                snapshot_date,
                batch,
                dry_run,
            } => import_cslb(config, file, source, county, snapshot_date, batch, dry_run).await,
            Command::LoadRegions { file, source } => load_regions(config, file, source).await,
            Command::SeedTrades => seed_trades(config).await,
            Command::RecomputeVerification => recompute_verification(config).await,
            Command::Prune {
                grace_days,
                audit_days,
                report_only,
            } => prune(config, grace_days, audit_days, report_only).await,
            Command::GeocodeWorker {
                once,
                provider,
                endpoint,
                batch,
                idle_secs,
            } => geocode_worker(config, once, provider, endpoint, batch, idle_secs).await,
            Command::CheckConfig => unreachable!("handled before the runtime starts"),
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "command failed");
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

async fn serve(config: Config) -> Result<(), cm_core::AppError> {
    for (key, value) in config.redacted_summary() {
        tracing::info!(target: "cm_server::config", key, value, "configuration");
    }

    let pool = cm_db::connect(&config.database).await?;

    // Serving deliberately does not migrate. A deploy runs `migrate` once and
    // then restarts; a server that migrates on boot would let two instances
    // race to change the schema during a rolling restart.
    //
    // A schema this binary cannot rely on is fatal, not a warning. Nothing in
    // the deployment guarantees that /readyz gates traffic — Caddy proxies to
    // this process whether or not it reports ready — so warning and continuing
    // would leave handlers answering requests against a schema they were not
    // written for. Refusing to start is what makes "migrate, then restart" the
    // only ordering that produces a serving process.
    //
    // A database *ahead* of this binary is explicitly allowed: migrations are
    // additive and backward-compatible, so the middle of a rolling deploy —
    // schema moved, this binary not yet replaced — is a valid state.
    // `blocking_reason` draws exactly that line, and /readyz calls the same
    // function, so the two can never disagree.
    let status = cm_db::migrate::status(&pool).await?;
    if let Some(reason) = status.blocking_reason() {
        pool.close().await;
        return Err(cm_core::AppError::unavailable(format!(
            "refusing to serve: {reason}. Run `cm-server migrate` first."
        )));
    }

    let state = cm_api::AppState::new(pool.clone(), &config)?;
    let router = cm_api::build(state);

    // Elapsed rate-limit windows are removed in bounded batches on a timer.
    // Doing it opportunistically inside a request would put a delete on the
    // latency path of whichever unlucky caller triggered it.
    let sweeper_pool = pool.clone();
    let sweeper = tokio::spawn(async move {
        let period = Duration::from_secs(cm_auth::ratelimit::SWEEP_INTERVAL_SECS);
        loop {
            tokio::time::sleep(period).await;
            match cm_auth::ratelimit::sweep(&sweeper_pool, chrono::Utc::now()).await {
                Ok(0) => {}
                Ok(removed) => tracing::debug!(removed, "swept expired rate-limit windows"),
                Err(error) => tracing::warn!(error = %error, "rate-limit sweep failed"),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .map_err(cm_core::AppError::internal)?;
    let local_addr = listener.local_addr().map_err(cm_core::AppError::internal)?;
    tracing::info!(%local_addr, environment = %config.environment, "listening");

    let grace = config.shutdown_grace;
    // `into_make_service_with_connect_info` is what makes the peer address
    // available to rate limiting. Without it every request would share one
    // bucket, and a per-IP limit would be a global one.
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(grace))
    .await
    .map_err(cm_core::AppError::internal)?;

    sweeper.abort();

    // Close the pool explicitly so in-flight statements finish before the
    // process exits, rather than being cut off by the runtime dropping.
    pool.close().await;
    tracing::info!("shutdown complete");
    Ok(())
}

async fn migrate(config: Config) -> Result<(), cm_core::AppError> {
    let pool = cm_db::connect(&config.database).await?;

    let before = cm_db::migrate::status(&pool).await?;
    tracing::info!(
        applied = ?before.applied,
        embedded = before.embedded,
        "applying migrations"
    );

    cm_db::migrate::run(&pool).await?;

    let after = cm_db::migrate::status(&pool).await?;
    tracing::info!(applied = ?after.applied, embedded = after.embedded, "migrations applied");
    pool.close().await;
    Ok(())
}

async fn admin(config: Config, command: AdminCommand) -> Result<(), cm_core::AppError> {
    use cm_db::repo::audit::{ActorKind, AuditEvent};
    use cm_db::repo::users;

    let pool = cm_db::connect(&config.database).await?;
    let mut conn = pool.acquire().await.map_err(cm_core::AppError::internal)?;

    let (email, role) = match &command {
        AdminCommand::GrantRole { email, role } | AdminCommand::RevokeRole { email, role } => {
            let parsed = users::Role::parse(role).ok_or_else(|| {
                cm_core::AppError::invalid(format!(
                    "unknown role \"{role}\"; expected one of {}",
                    users::Role::ALL
                        .iter()
                        .map(|role| role.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            (email.clone(), Some(parsed))
        }
        AdminCommand::ShowRoles { email } => (email.clone(), None),
    };

    let user = users::find_by_email(&mut conn, email.trim())
        .await?
        .ok_or_else(|| cm_core::AppError::invalid(format!("no account for {email}")))?;

    match command {
        AdminCommand::ShowRoles { .. } => {
            let roles = users::roles(&mut conn, user.id).await?;
            if roles.is_empty() {
                println!("{} ({}) has no roles", user.email, user.id);
            } else {
                println!(
                    "{} ({}): {}",
                    user.email,
                    user.id,
                    roles
                        .iter()
                        .map(|role| role.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        AdminCommand::GrantRole { .. } | AdminCommand::RevokeRole { .. } => {
            let role = role.expect("a role was parsed above");
            let granting = matches!(command, AdminCommand::GrantRole { .. });

            let mut tx = pool.begin().await.map_err(cm_core::AppError::internal)?;
            let changed = if granting {
                // `granted_by` is NULL: the operator at a shell is not one of
                // our accounts, and inventing a user id for them would make the
                // audit trail say something untrue.
                users::grant_role(&mut tx, user.id, role, None).await?
            } else {
                users::revoke_role(&mut tx, user.id, role).await?
            };

            if changed {
                cm_db::repo::audit::record(
                    &mut tx,
                    AuditEvent::new(
                        if granting {
                            "auth.role_granted"
                        } else {
                            "auth.role_revoked"
                        },
                        "users",
                    )
                    .actor(ActorKind::Admin, None)
                    .subject(user.id)
                    .data(serde_json::json!({ "role": role.as_str(), "via": "cli" })),
                )
                .await?;
            }
            tx.commit().await.map_err(cm_core::AppError::internal)?;

            let verb = if granting { "granted" } else { "revoked" };
            if changed {
                println!("{verb} {role} for {} ({})", user.email, user.id);
            } else {
                println!("no change: {} already reflects {role} {verb}", user.email);
            }
        }
    }

    // Before closing: `close` waits for every connection to be returned, and a
    // pooled connection still in scope never is. Holding one here would hang
    // the process after it had already printed its result.
    drop(conn);
    pool.close().await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn import_cslb(
    config: Config,
    file: std::path::PathBuf,
    source: String,
    county: Option<String>,
    snapshot_date: Option<String>,
    batch: usize,
    dry_run: bool,
) -> Result<(), cm_core::AppError> {
    let source = cm_db::repo::licenses::Source::parse(&source).ok_or_else(|| {
        cm_core::AppError::invalid("unknown source; expected cslb_master_list or cslb_county_list")
    })?;
    let snapshot_date = snapshot_date
        .map(|value| {
            chrono::NaiveDate::parse_from_str(&value, "%Y-%m-%d")
                .map_err(|_| cm_core::AppError::invalid("snapshot date must be YYYY-MM-DD"))
        })
        .transpose()?;

    let pool = cm_db::connect(&config.database).await?;
    let options = cm_domain::import::ImportOptions {
        source,
        file_path: file,
        county,
        snapshot_date,
        batch_size: batch.clamp(1, 5_000),
        dry_run,
    };

    let counts = cm_domain::import::run(&pool, &options).await;
    pool.close().await;
    let counts = counts?;

    println!(
        "read {} · inserted {} · updated {} · unchanged {} · skipped {} · rejected {}{}",
        counts.read,
        counts.inserted,
        counts.updated,
        counts.unchanged,
        counts.skipped,
        counts.rejected,
        if dry_run {
            "  (dry run: nothing written)"
        } else {
            ""
        }
    );
    Ok(())
}

async fn load_regions(
    config: Config,
    file: std::path::PathBuf,
    source: String,
) -> Result<(), cm_core::AppError> {
    let pool = cm_db::connect(&config.database).await?;
    let result = load_regions_inner(&pool, &file, &source).await;
    pool.close().await;
    let loaded = result?;

    println!("loaded {loaded} ZIP-code centroids from {}", file.display());
    Ok(())
}

async fn load_regions_inner(
    pool: &cm_db::PgPool,
    file: &std::path::Path,
    source: &str,
) -> Result<u64, cm_core::AppError> {
    #[derive(serde::Deserialize)]
    struct Row {
        code: String,
        name: String,
        lat: f64,
        lon: f64,
    }

    let handle = std::fs::File::open(file)
        .map_err(|e| cm_core::AppError::invalid(format!("cannot read {}: {e}", file.display())))?;
    let mut reader = csv::Reader::from_reader(std::io::BufReader::new(handle));
    let mut loaded = 0;
    let mut tx = pool.begin().await.map_err(cm_core::AppError::internal)?;

    for row in reader.deserialize::<Row>() {
        let row =
            row.map_err(|e| cm_core::AppError::invalid(format!("malformed region row: {e}")))?;
        if !(-90.0..=90.0).contains(&row.lat) || !(-180.0..=180.0).contains(&row.lon) {
            return Err(cm_core::AppError::invalid(format!(
                "region {} has an impossible centroid",
                row.code
            )));
        }
        cm_db::repo::reference::upsert_zcta(
            &mut tx, &row.code, &row.name, row.lat, row.lon, source,
        )
        .await?;
        loaded += 1;
    }

    tx.commit().await.map_err(cm_core::AppError::internal)?;
    Ok(loaded)
}

async fn seed_trades(config: Config) -> Result<(), cm_core::AppError> {
    let pool = cm_db::connect(&config.database).await?;
    let mut conn = pool.acquire().await.map_err(cm_core::AppError::internal)?;
    let (written, total) = cm_db::repo::reference::seed_trades(&mut conn).await?;

    // Seeding the taxonomy is only half of it: contractors already imported
    // carry whatever trades the taxonomy held when they were imported, and
    // re-importing will not revisit them because an unchanged licence
    // short-circuits. Deriving here is what makes a taxonomy change reach the
    // directory.
    let aliases = cm_db::repo::reference::seed_trade_aliases(&mut conn).await?;
    let rederived = cm_db::repo::reference::rederive_cslb_trades(&mut conn).await;
    drop(conn);
    pool.close().await;

    let (added, removed) = rederived?;
    println!("wrote {written} of {total} trade(s), {aliases} alias(es)");
    println!("re-derived contractor trades: {added} added, {removed} removed");
    Ok(())
}

async fn recompute_verification(config: Config) -> Result<(), cm_core::AppError> {
    let pool = cm_db::connect(&config.database).await?;
    let processed = cm_domain::verification::recompute_all(&pool, 500).await;
    pool.close().await;

    println!("recomputed {} contractor(s)", processed?);
    Ok(())
}

async fn prune(
    config: Config,
    grace_days: i64,
    audit_days: Option<i64>,
    report_only: bool,
) -> Result<(), cm_core::AppError> {
    let pool = cm_db::connect(&config.database).await?;

    let before = cm_domain::maintenance::growth_report(&pool).await;
    let result = if report_only {
        Ok(cm_db::repo::maintenance::Pruned::default())
    } else {
        cm_domain::maintenance::prune(
            &pool,
            chrono::Utc::now(),
            grace_days.clamp(1, 3650),
            audit_days.map(|days| days.clamp(1, 3650)),
        )
        .await
    };
    let after = cm_domain::maintenance::growth_report(&pool).await;
    pool.close().await;

    println!("table rows (before -> after):");
    let after = after?;
    for (table, rows) in before? {
        let now = after
            .iter()
            .find(|(name, _)| name == &table)
            .map(|(_, rows)| *rows)
            .unwrap_or(rows);
        println!("  {table:<24} {rows:>8} -> {now}");
    }

    let pruned = result?;
    if !report_only {
        println!(
            "pruned: {} session(s), {} geocode job(s), {} rate-limit window(s), {} audit row(s)",
            pruned.sessions, pruned.geocode_jobs, pruned.rate_limit_windows, pruned.audit_rows
        );
    }

    Ok(())
}

async fn geocode_worker(
    config: Config,
    once: bool,
    provider: String,
    endpoint: Option<String>,
    batch: i64,
    idle_secs: u64,
) -> Result<(), cm_core::AppError> {
    let geocoder = cm_domain::geocoder::build(&provider, endpoint)?;
    let pool = cm_db::connect(&config.database).await?;

    let worker_config = cm_domain::geocode_worker::WorkerConfig {
        batch: batch.clamp(1, 200),
        worker_id: format!("geocode-worker-{}", std::process::id()),
        ..Default::default()
    };

    // The loop exits on SIGTERM between passes, so a deploy never interrupts a
    // pass halfway and leaves rows claimed.
    let shutdown = tokio::sync::Notify::new();
    let result = tokio::select! {
        result = async {
            loop {
                let stats =
                    cm_domain::geocode_worker::run_once(&pool, &geocoder, &worker_config).await?;
                tracing::info!(
                    claimed = stats.claimed,
                    located = stats.located,
                    not_found = stats.not_found,
                    failed = stats.failed,
                    skipped = stats.skipped,
                    requeued = stats.requeued,
                    "geocoding pass complete"
                );

                if once {
                    return Ok::<(), cm_core::AppError>(());
                }
                if stats.claimed == 0 {
                    tokio::time::sleep(Duration::from_secs(idle_secs.clamp(1, 3600))).await;
                }
            }
        } => result,
        () = shutdown_signal(config.shutdown_grace) => {
            tracing::info!("stopping the geocoding worker");
            Ok(())
        }
    };
    shutdown.notify_waiters();

    pool.close().await;
    result
}

/// Resolves on SIGTERM (systemd stop) or SIGINT (Ctrl-C).
///
/// `grace` bounds how long axum is given to drain: without a bound, one stuck
/// request holds the old process open through the whole deploy.
async fn shutdown_signal(grace: Duration) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install the Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install the SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("received SIGINT, draining"),
        () = terminate => tracing::info!("received SIGTERM, draining"),
    }

    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        tracing::warn!(
            grace_secs = grace.as_secs(),
            "grace period elapsed with requests still in flight; exiting anyway"
        );
        std::process::exit(0);
    });
}
