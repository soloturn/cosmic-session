// SPDX-License-Identifier: GPL-3.0-only
#[macro_use]
extern crate tracing;

mod comp;
mod process;
mod service;
mod systemd;

use async_signals::Signals;
use color_eyre::{eyre::WrapErr, Result};
use futures_util::StreamExt;
use launch_pad::{process::Process, ProcessManager};
use tokio::{
	sync::{mpsc, oneshot},
	time::{sleep, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::{metadata::LevelFilter, Instrument};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use zbus::ConnectionBuilder;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
	color_eyre::install().wrap_err("failed to install color_eyre error handler")?;

	tracing_subscriber::registry()
		.with(tracing_journald::layer().wrap_err("failed to connect to journald")?)
		.with(fmt::layer())
		.with(
			EnvFilter::builder()
				.with_default_directive(LevelFilter::INFO.into())
				.from_env_lossy(),
		)
		.try_init()
		.wrap_err("failed to initialize logger")?;
	log_panics::init();

	info!("Starting cosmic-session");

	let process_manager = ProcessManager::new().await;
	let token = CancellationToken::new();
	let (_, socket_rx) = mpsc::unbounded_channel();
	let (env_tx, env_rx) = oneshot::channel();
	let compositor_handle =
		comp::run_compositor(&process_manager, token.child_token(), socket_rx, env_tx)
			.wrap_err("failed to start compositor")?;
	sleep(Duration::from_millis(2000)).await;
	systemd::start_systemd_target()
		.await
		.wrap_err("failed to start systemd target")?;
	// Always stop the target when the process exits or panics.
	scopeguard::defer! {
		if let Err(error) = systemd::stop_systemd_target() {
			error!("failed to stop systemd target: {:?}", error);
		}
	}
	let env_vars = env_rx
		.await
		.expect("failed to receive environmental variables")
		.into_iter()
		.collect::<Vec<_>>();
	info!("got environmental variables: {:?}", env_vars);

	process_manager
		.start(
			Process::new()
				.with_executable("cosmic-panel")
				.with_env(env_vars.clone()),
		)
		.await
		.expect("failed to start panel");

	let span = info_span!(parent: None, "cosmic-app-library");
	start_component("cosmic-app-library", span, &process_manager, &env_vars).await;

	let span = info_span!(parent: None, "cosmic-launcher");
	start_component("cosmic-launcher", span, &process_manager, &env_vars).await;

	let span = info_span!(parent: None, "cosmic-workspaces");
	start_component("cosmic-workspaces", span, &process_manager, &env_vars).await;

	let span = info_span!(parent: None, "cosmic-osd");
	start_component("cosmic-osd", span, &process_manager, &env_vars).await;

	let span = info_span!(parent: None, "cosmic-bg");
	start_component("cosmic-bg", span, &process_manager, &env_vars).await;

	let span = info_span!(parent: None, "xdg-desktop-portal-cosmic");
	start_component(
		"/usr/lib/xdg-desktop-portal-cosmic",
		span,
		&process_manager,
		&env_vars,
	)
	.await;

	process_manager
		.start(Process::new().with_executable("cosmic-settings-daemon"))
		.await
		.expect("failed to start settings daemon");

	let (exit_tx, exit_rx) = oneshot::channel();
	let _ = ConnectionBuilder::session()?
		.name("com.system76.CosmicSession")?
		.serve_at("/com/system76/CosmicSession", service::SessionService {
			exit_tx: Some(exit_tx),
		})?
		.build()
		.await?;

	let mut signals = Signals::new(vec![libc::SIGTERM, libc::SIGINT]).unwrap();
	loop {
		tokio::select! {
			_ = compositor_handle => {
				info!("compositor exited");
				break;
			},
			_ = exit_rx => {
				info!("session exited by request");
				break;
			},
			signal = signals.next() => match signal {
				Some(libc::SIGTERM | libc::SIGINT) => {
					info!("received request to terminate");
					break;
				}
				Some(signal) => unreachable!("received unhandled signal {}", signal),
				None => break,
			}
		}
	}
	token.cancel();
	tokio::time::sleep(std::time::Duration::from_secs(2)).await;
	Ok(())
}

async fn start_component(
	cmd: &str,
	span: tracing::Span,
	process_manager: &ProcessManager,
	env_vars: &[(String, String)],
) {
	let stdout_span = span.clone();
	let stderr_span = span.clone();
	process_manager
		.start(
			Process::new()
				.with_executable(cmd)
				.with_env(env_vars.iter().cloned())
				.with_on_stdout(move |_, _, line| {
					let stdout_span = stdout_span.clone();
					async move {
						info!("{}", line);
					}
					.instrument(stdout_span)
				})
				.with_on_stderr(move |_, _, line| {
					let stderr_span = stderr_span.clone();
					async move {
						warn!("{}", line);
					}
					.instrument(stderr_span)
				}),
		)
		.await
		.expect(&format!("failed to start {}", cmd));
}
