use crate::authn;
use crate::authn::JwtAuthenticator;
use crate::proto;
use crate::proto::aidp::dev::listener::{
	Listener as XdsListener, SseListener as XdsSseListener, listener::Listener as XdsListenerSpec,
	sse_listener::TlsConfig as XdsTlsConfig,
};
use crate::proxyprotocol;
use crate::rbac;
use crate::relay;
use crate::sse::App as SseApp;
use crate::xds;
use rmcp::service::serve_server_with_ct;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::task::AbortHandle;
use tokio_rustls::{
	TlsAcceptor,
	rustls::ServerConfig,
	rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use tracing::info;

#[derive(Clone, Serialize, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Listener {
	#[serde(rename = "sse")]
	Sse(SseListener),
	#[serde(rename = "stdio")]
	Stdio,
}

impl Listener {
	pub async fn from_xds(value: XdsListener) -> Result<Self, anyhow::Error> {
		Ok(match value.listener {
			Some(XdsListenerSpec::Sse(sse)) => Listener::Sse(SseListener::from_xds(sse).await?),
			Some(XdsListenerSpec::Stdio(_)) => Listener::Stdio,
			_ => Listener::Stdio,
		})
	}
}

#[derive(Clone, Serialize, Debug)]

pub struct SseListener {
	host: String,
	port: u32,
	mode: Option<ListenerMode>,
	authn: Option<JwtAuthenticator>,
	tls: Option<TlsConfig>,
	rbac: Vec<rbac::RuleSet>,
}

impl SseListener {
	async fn from_xds(value: XdsSseListener) -> Result<Self, anyhow::Error> {
		let tls = match value.tls {
			Some(tls) => Some(from_xds_tls_config(tls)?),
			None => None,
		};
		let authn = match value.authn {
			Some(authn) => match authn.jwt {
				Some(jwt) => Some(
					JwtAuthenticator::new(&jwt)
						.await
						.map_err(|e| anyhow::anyhow!("error creating jwt authenticator: {:?}", e))?,
				),
				None => None,
			},
			None => None,
		};
		let rbac = value
			.rbac
			.iter()
			.map(rbac::RuleSet::try_from)
			.collect::<Result<Vec<rbac::RuleSet>, anyhow::Error>>()?;
		Ok(SseListener {
			host: value.address,
			port: value.port,
			mode: None,
			authn,
			tls,
			rbac,
		})
	}
}
#[derive(Clone, Debug)]
pub struct TlsConfig {
	pub(crate) inner: Arc<ServerConfig>,
}

// TODO: Implement Serialize for TlsConfig
impl Serialize for TlsConfig {
	fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		todo!()
	}
}

fn from_xds_tls_config(value: XdsTlsConfig) -> Result<TlsConfig, anyhow::Error> {
	let cert_bytes = value
		.cert_pem
		.ok_or(anyhow::anyhow!("cert_pem is required"))?
		.source
		.ok_or(anyhow::anyhow!("cert_pem source is required"))?;
	let key_bytes = value
		.key_pem
		.ok_or(anyhow::anyhow!("key_pem is required"))?
		.source
		.ok_or(anyhow::anyhow!("key_pem source is required"))?;
	let cert = proto::resolve_local_data_source(&cert_bytes)?;
	let key = proto::resolve_local_data_source(&key_bytes)?;
	Ok(TlsConfig {
		inner: rustls_server_config(key, cert)?,
	})
}

fn rustls_server_config(
	key: impl AsRef<Vec<u8>>,
	cert: impl AsRef<Vec<u8>>,
) -> Result<Arc<ServerConfig>, anyhow::Error> {
	let key = PrivateKeyDer::from_pem_slice(key.as_ref())?;

	let certs = CertificateDer::pem_slice_iter(cert.as_ref())
		.map(|cert| cert.unwrap())
		.collect();

	let mut config = ServerConfig::builder()
		.with_no_client_auth()
		.with_single_cert(certs, key)?;

	config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

	Ok(Arc::new(config))
}

#[derive(Debug)]
pub enum ServingError {
	Sse(std::io::Error),
	StdIo(tokio::task::JoinError),
}

impl Listener {
	pub async fn listen(
		&self,
		state: Arc<tokio::sync::RwLock<xds::XdsStore>>,
		metrics: Arc<relay::metrics::Metrics>,
		ct: tokio_util::sync::CancellationToken,
	) -> Result<(), ServingError> {
		match self {
			Listener::Stdio => {
				let relay = serve_server_with_ct(
					relay::Relay::new(
						state.clone(),
						metrics,
						Default::default(),
						"stdio".to_string(),
					),
					(tokio::io::stdin(), tokio::io::stdout()),
					ct,
				)
				.await
				.inspect_err(|e| {
					tracing::error!("serving error: {:?}", e);
				})
				.unwrap();
				tracing::info!("serving stdio");
				relay
					.waiting()
					.await
					.map_err(ServingError::StdIo)
					.map(|_| ())
					.inspect_err(|e| {
						tracing::error!("serving error: {:?}", e);
					})
			},
			Listener::Sse(sse_listener) => {
				let authenticator = match &sse_listener.authn {
					Some(authn) => Arc::new(tokio::sync::RwLock::new(Some(authn.clone()))),
					None => Arc::new(tokio::sync::RwLock::new(None)),
				};

				let mut run_set: tokio::task::JoinSet<Result<(), anyhow::Error>> =
					tokio::task::JoinSet::new();
				let clone = authenticator.clone();

				let child_token = ct.child_token();
				run_set.spawn(async move {
					authn::sync_jwks_loop(clone, child_token)
						.await
						.map_err(|e| anyhow::anyhow!("error syncing jwks: {:?}", e))
				});

				let socket_addr: SocketAddr = format!("{}:{}", sse_listener.host, sse_listener.port)
					.as_str()
					.parse()
					.unwrap();
				let listener = tokio::net::TcpListener::bind(socket_addr).await.unwrap();
				let child_token = ct.child_token();
				let app = SseApp::new(state.clone(), metrics, authenticator, child_token, sse_listener.rbac.clone(), "sse".to_string());
				let router = app.router();

				info!("serving sse on {}:{}", sse_listener.host, sse_listener.port);
				let child_token = ct.child_token();
				match &sse_listener.tls {
					Some(tls) => {
						let tls_acceptor = TlsAcceptor::from(tls.inner.clone());
						let axum_tls_acceptor = proxyprotocol::AxumTlsAcceptor::new(tls_acceptor);
						let tls_listener = proxyprotocol::AxumTlsListener::new(
							tls_listener::TlsListener::new(axum_tls_acceptor, listener),
							socket_addr,
							Some(&ListenerMode::Proxy) == sse_listener.mode.as_ref(),
						);

						let svc: axum::extract::connect_info::IntoMakeServiceWithConnectInfo<
							axum::Router,
							proxyprotocol::Address,
						> = router.into_make_service_with_connect_info::<proxyprotocol::Address>();
						run_set.spawn(async move {
							axum::serve(tls_listener, svc)
								.with_graceful_shutdown(async move {
									child_token.cancelled().await;
								})
								.await
								.map_err(ServingError::Sse)
								.inspect_err(|e| {
									tracing::error!("serving error: {:?}", e);
								})
								.map_err(|e| anyhow::anyhow!("serving error: {:?}", e))
						});
					},
					None => {
						let enable_proxy = Some(&ListenerMode::Proxy) == sse_listener.mode.as_ref();

						let listener = proxyprotocol::Listener::new(listener, enable_proxy);
						let svc: axum::extract::connect_info::IntoMakeServiceWithConnectInfo<
							axum::Router,
							proxyprotocol::Address,
						> = router.into_make_service_with_connect_info::<proxyprotocol::Address>();
						run_set.spawn(async move {
							axum::serve(listener, svc)
								.with_graceful_shutdown(async move {
									child_token.cancelled().await;
								})
								.await
								.map_err(ServingError::Sse)
								.inspect_err(|e| {
									tracing::error!("serving error: {:?}", e);
								})
								.map_err(|e| anyhow::anyhow!("serving error: {:?}", e))
						});
					},
				}

				while let Some(res) = run_set.join_next().await {
					match res {
						Ok(_) => {},
						Err(e) => {
							tracing::error!("serving error: {:?}", e);
						},
					}
				}
				Ok(())
			},
		}
	}
}

#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
pub enum ListenerMode {
	#[serde(rename = "proxy")]
	Proxy,
}

impl Default for Listener {
	fn default() -> Self {
		Self::Stdio {}
	}
}

pub struct ListenerManager {
	state: Arc<tokio::sync::RwLock<xds::XdsStore>>,
	update_rx: tokio::sync::mpsc::Receiver<xds::UpdateEvent>,
	metrics: Arc<relay::metrics::Metrics>,

	running: HashMap<String, AbortHandle>,
	run_set: tokio::task::JoinSet<Result<(), ServingError>>,
}

impl ListenerManager {
	// Start all listeners in the state
	// Consider these to be "static" listeners
	pub async fn new(
		ct: tokio_util::sync::CancellationToken,
		state: Arc<tokio::sync::RwLock<xds::XdsStore>>,
		update_rx: tokio::sync::mpsc::Receiver<xds::UpdateEvent>,
		metrics: Arc<relay::metrics::Metrics>,
	) -> Self {
		// Start all listeners in the state
		// Consider these to be "static" listeners
		let mut run_set = tokio::task::JoinSet::new();
		let mut running: HashMap<String, AbortHandle> = HashMap::new();

		for (name, listener) in state.read().await.listeners.iter() {
			let state_clone = state.clone();
			let metrics_clone = metrics.clone();
			let ct_clone = ct.clone();
			let listener_clone = listener.clone();
			let listener_name = name.clone();
			let abort_handle = run_set.spawn(async move {
				listener_clone
					.listen(state_clone, metrics_clone, ct_clone)
					.await
			});
			running.insert(listener_name, abort_handle);
		}

		// Start the update loop

		Self {
			state,
			update_rx,
			metrics,
			running,
			run_set,
		}
	}
}

impl ListenerManager {
	pub async fn run(
		&mut self,
		ct: tokio_util::sync::CancellationToken,
	) -> Result<(), anyhow::Error> {
		loop {
			tokio::select! {
				result = self.run_set.join_next() => {
					tracing::info!("run_set join_next returned {:?}", result);
				}
				update = self.update_rx.recv() => {
					match update {
						Some(xds::UpdateEvent::Insert(name)) => {
							// Scope the read lock to get the listener and clone it
							let listener_clone: Listener = {
									let state = self.state.read().await;
									// Check if listener exists before trying to clone
									match state.listeners.get(&name).await {
											Ok(listener_ref) => listener_ref.clone(), // Clone the listener
											Err(e) => {
													tracing::error!("Failed to get listener {} from state: {}", name, e);
													continue; // Skip spawning if listener fetch failed
											}
									}
							}; // Read lock dropped here

							// Now use the owned listener_clone for spawning
							let state_clone = self.state.clone();
							let metrics_clone = self.metrics.clone();
							let ct_clone = ct.clone();

							// Spawn the task with the cloned listener and other cloned Arcs
							let abort_handle = self.run_set.spawn(async move { // Add async move
									listener_clone.listen(state_clone, metrics_clone, ct_clone).await
							});
							self.running.insert(name, abort_handle); // Store the JoinHandle
						}
						Some(xds::UpdateEvent::Remove(name)) => {
								if let Some(handle) = self.running.remove(&name) {
										handle.abort(); // Abort the task associated with the removed listener
										tracing::info!("Aborted listener task for: {}", name);
								} else {
										tracing::warn!("Received remove event for {}, but no running task found.", name);
								}
						}
						None => {
							tracing::info!("update_rx closed");
							break;
						}
					}
				}
				_ = ct.cancelled() => {
					break;
				}
			}
		}

		self.run_set.shutdown().await;
		Ok(())
	}
}
