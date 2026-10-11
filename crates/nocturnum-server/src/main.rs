//! The nocturnum binary, which serves the API and the embedded console on one port.

mod config;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use nocturnum_api::{Api, ApiOptions, Registry, RegistryOptions};
use nocturnum_storage::OptimizerOptions;
use tokio::sync::oneshot;

use crate::config::{Cli, Config, Env};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = match Config::load(&cli, Env::read()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nocturnum: {e}");
            return ExitCode::from(2);
        }
    };
    if cli.print_config {
        print!("{}", config.redacted());
        return ExitCode::SUCCESS;
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&config.log.level))
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    tracing::info!(
        "nocturnum {} starting with this config:\n{}",
        env!("CARGO_PKG_VERSION"),
        config.redacted()
    );
    match serve(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn serve(config: Config) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("starting the runtime: {e}"))?;
    let api = Api::new(ApiOptions {
        admin_key: config.auth.admin_key.clone(),
        read_only_key: config.auth.read_only_key.clone(),
        max_body_bytes: config.server.max_body_bytes,
        allowed_hosts: config.server.allowed_hosts.clone(),
    });
    let result = runtime.block_on(async {
        let addr = config.addr();
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("listening on {addr}: {e}"))?;
        tracing::info!(
            "listening on http://{}",
            listener.local_addr().unwrap_or(addr)
        );
        if config.auth.admin_key.is_none() {
            tracing::warn!("no API keys are set, so anyone who can reach {addr} has full access");
        }

        // Collections load after binding, so health checks answer during a long recovery.
        let (failed, load_failed) = oneshot::channel::<String>();
        let loader = api.clone();
        let data = config.storage.path.clone();
        tokio::task::spawn_blocking(move || {
            let options = RegistryOptions {
                shard: Default::default(),
                optimizer: Some(OptimizerOptions::default()),
            };
            match Registry::open(&data, options) {
                Ok(r) => {
                    tracing::info!("{} collections loaded, ready", r.list().len());
                    loader.set_registry(r);
                }
                Err(e) => {
                    let _ = failed.send(format!("loading {}: {}", data.display(), e.message));
                }
            }
        });

        let stop = tokio::spawn(async move {
            tokio::select! {
                () = shutdown_signal() => None,
                Ok(e) = load_failed => Some(e),
            }
        });
        let (done, finished) = oneshot::channel::<()>();
        let watcher = tokio::spawn(async move {
            let why = stop.await.ok().flatten();
            tracing::info!("shutting down");
            let _ = done.send(());
            why
        });
        axum::serve(listener, api.router())
            .with_graceful_shutdown(async {
                let _ = finished.await;
            })
            .await
            .map_err(|e| format!("serving: {e}"))?;
        match watcher.await.ok().flatten() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    });
    // Waits for work still running on blocking threads, then closes the collections.
    drop(runtime);
    drop(api);
    result
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
