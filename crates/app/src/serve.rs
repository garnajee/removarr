//! `removarr serve`: configuration, logging, wiring, job runtime, HTTP server, graceful
//! shutdown (architecture 8.2): stop accepting HTTP, cancel jobs, wait up to
//! `server.shutdown_grace_seconds`, close the database.

use std::net::SocketAddr;
use std::time::Duration;

use removarr_config::Config;

use crate::{logging, wiring};

/// Exit code of a startup failure after a valid configuration.
pub const EXIT_STARTUP: u8 = 1;

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!("shutdown requested");
}

/// Runs the server until a shutdown signal. Returns the process exit code.
pub async fn run(cfg: Config) -> u8 {
    let logs = logging::buffer(&cfg);
    logging::install(&logs);
    for line in cfg.startup_summary() {
        tracing::info!("{line}");
    }
    let app = match wiring::build(&cfg, logs, wiring::default_guard()).await {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("startup failed: {e}");
            return EXIT_STARTUP;
        }
    };
    tracing::info!(
        mode = ?app.report.mode,
        live_adapters = app.report.live_adapters,
        auto_mode = app.report.auto_mode,
        "wiring complete"
    );
    if let Err(e) = app.probe.check_available().await {
        tracing::warn!(class = ?e.class, "ffprobe is not available; replacement checks will be blocked");
    }
    match app.runtime.start().await {
        Ok(r) => tracing::info!(
            interrupted = r.interrupted.len(),
            requeued = r.requeued.len(),
            auto_paused = r.auto_paused.is_some(),
            "job runtime started"
        ),
        Err(e) => {
            tracing::error!(class = ?e.class, "job runtime could not start");
            return EXIT_STARTUP;
        }
    }
    let listener = match tokio::net::TcpListener::bind(cfg.server.bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(kind = ?e.kind(), "cannot bind the HTTP listener");
            app.runtime.shutdown(Duration::from_secs(5)).await;
            return EXIT_STARTUP;
        }
    };
    tracing::info!(bind = %cfg.server.bind, "listening");
    let router = removarr_web::build(app.state.clone());
    let served = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await;
    if let Err(e) = served {
        tracing::error!(kind = ?e.kind(), "HTTP server error");
    }
    app.runtime
        .shutdown(Duration::from_secs(cfg.server.shutdown_grace_seconds))
        .await;
    app.db.close().await;
    tracing::info!("stopped");
    0
}
