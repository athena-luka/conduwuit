extern crate conduwuit_admin as admin;
extern crate conduwuit_core as conduwuit;
extern crate conduwuit_service as service;

use std::{
	sync::{atomic::Ordering, Arc, Weak},
	time::Duration,
};

use axum_server::Handle as ServerHandle;
use conduwuit::{debug, debug_error, debug_info, error, info, Error, Result, Server};
use service::Services;
use tokio::{
	sync::broadcast::{self, Sender},
	task::JoinHandle,
};

use crate::serve;

/// Main loop base
#[tracing::instrument(skip_all)]
pub(crate) async fn run(services: Arc<Services>) -> Result<()> {
	let server = &services.server;
	debug!("Start");

	// Install the admin room callback here for now
	admin::init(&services.admin).await;

	// Setup shutdown/signal handling
	let handle = ServerHandle::new();
	let (tx, _) = broadcast::channel::<()>(1);
	let sigs = server
		.runtime()
		.spawn(signal(server.clone(), tx.clone(), handle.clone()));

	let mut listener =
		server
			.runtime()
			.spawn(serve::serve(services.clone(), handle.clone(), tx.subscribe()));

	// Focal point
	debug!("Running");
	let res = tokio::select! {
		res = &mut listener => res.map_err(Error::from).unwrap_or_else(Err),
		res = services.poll() => handle_services_poll(server, res, listener).await,
	};

	// Join the signal handler before we leave.
	sigs.abort();
	_ = sigs.await;

	// Remove the admin room callback
	admin::fini(&services.admin).await;

	debug_info!("Finish");
	res
}

/// Async initializations
#[tracing::instrument(skip_all)]
pub(crate) async fn start(server: Arc<Server>) -> Result<Arc<Services>> {
	debug!("Starting...");

	let services = Services::build(server).await?.start().await?;

	#[cfg(all(feature = "systemd", target_os = "linux"))]
	sd_notify::notify(true, &[sd_notify::NotifyState::Ready])
		.expect("failed to notify systemd of ready state");

	debug!("Started");
	Ok(services)
}

/// Async destructions
#[tracing::instrument(skip_all)]
pub(crate) async fn stop(services: Arc<Services>) -> Result<()> {
	debug!("Shutting down...");

	// Wait for all completions before dropping or we'll lose them to the module
	// unload and explode.
	services.stop().await;

	// Check that Services and Database will drop as expected, The complex of Arc's
	// used for various components can easily lead to references being held
	// somewhere improperly; this can hang shutdowns.
	debug!("Cleaning up...");
	let db = Arc::downgrade(&services.db);
	if let Err(services) = Arc::try_unwrap(services) {
		debug_error!(
			"{} dangling references to Services after shutdown",
			Arc::strong_count(&services)
		);
	}

	if Weak::strong_count(&db) > 0 {
		debug_error!(
			"{} dangling references to Database after shutdown",
			Weak::strong_count(&db)
		);
	}

	#[cfg(all(feature = "systemd", target_os = "linux"))]
	sd_notify::notify(true, &[sd_notify::NotifyState::Stopping])
		.expect("failed to notify systemd of stopping state");

	info!("Shutdown complete.");
	Ok(())
}

#[tracing::instrument(skip_all)]
async fn signal(server: Arc<Server>, tx: Sender<()>, handle: axum_server::Handle) {
	loop {
		let sig: &'static str = server
			.signal
			.subscribe()
			.recv()
			.await
			.expect("channel error");

		if !server.running() {
			handle_shutdown(&server, &tx, &handle, sig).await;
			break;
		}
	}
}

async fn handle_shutdown(
	server: &Arc<Server>,
	tx: &Sender<()>,
	handle: &axum_server::Handle,
	sig: &str,
) {
	debug!("Received signal {sig}");
	if let Err(e) = tx.send(()) {
		error!("failed sending shutdown transaction to channel: {e}");
	}

	let timeout = Duration::from_secs(36);
	debug!(
		?timeout,
		spawn_active = ?server.metrics.requests_spawn_active.load(Ordering::Relaxed),
		handle_active = ?server.metrics.requests_handle_active.load(Ordering::Relaxed),
		"Notifying for graceful shutdown"
	);

	handle.graceful_shutdown(Some(timeout));
}

async fn handle_services_poll(
	server: &Arc<Server>,
	result: Result<()>,
	listener: JoinHandle<Result<()>>,
) -> Result<()> {
	debug!("Service manager finished: {result:?}");

	if server.running() {
		if let Err(e) = server.shutdown() {
			error!("Failed to send shutdown signal: {e}");
		}
	}

	if let Err(e) = listener.await {
		error!("Client listener task finished with error: {e}");
	}

	result
}
