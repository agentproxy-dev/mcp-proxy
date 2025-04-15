use anyhow::Result;
use clap::Parser;
use mcp_proxy::r#static::{StaticConfig, run_local_client};
use mcp_proxy::xds::Config as XdsConfig;
use prometheus_client::registry::Registry;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::task::JoinSet;
use tracing_subscriber::{self, EnvFilter};

use mcp_proxy::admin;
use mcp_proxy::mtrcs;
use mcp_proxy::proto::aidp::dev::listener::Listener as XdsListener;
use mcp_proxy::proto::aidp::dev::mcp::target::Target as XdsTarget;
use mcp_proxy::relay;
use mcp_proxy::signal;
use mcp_proxy::trcng;
use mcp_proxy::xds;
use mcp_proxy::xds::ProxyStateUpdater;
use mcp_proxy::xds::XdsStore as ProxyState;
use mcp_proxy::{a2a, inbound};
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
	/// Use config from bytes
	#[arg(short, long, value_name = "config")]
	config: Option<bytes::Bytes>,

	/// Use config from file
	#[arg(short, long, value_name = "file")]
	file: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ConfigType {
	#[serde(rename = "static")]
	Static(StaticConfig),
	#[serde(rename = "xds")]
	Xds(XdsConfig),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
	#[serde(flatten)]
	pub config_type: ConfigType,

	pub admin: Option<admin::Config>,
	pub metrics: Option<mtrcs::Config>,
	pub tracing: Option<trcng::Config>,
}

#[tokio::main]
async fn main() -> Result<()> {
	// Initialize logging
	// Initialize the tracing subscriber with file and stdout logging
	tracing_subscriber::fmt()
		.with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
		.with_writer(std::io::stderr)
		.with_ansi(false)
		.init();

	let mut registry = Registry::default();

	let args = Args::parse();

	// TODO: Do this better
	rustls::crypto::ring::default_provider()
		.install_default()
		.expect("failed to install ring provider");

	let cfg: Config = match (args.file, args.config) {
		(Some(filename), None) => {
			// If filename is a URL, download it
			match reqwest::Url::parse(&filename) {
				Ok(url) => {
					println!("Downloading config from URL: {}", url);
					let response = reqwest::get(url).await?;
					let body = response.text().await?;
					serde_json::from_str(&body)?
				},
				Err(_) => {
					println!("Reading config from file: {}", filename);
					let file = tokio::fs::read_to_string(filename).await?;
					serde_json::from_str(&file)?
				},
			}
		},
		(None, Some(config)) => {
			let file = std::str::from_utf8(&config).map(|s| s.to_string())?;
			serde_json::from_str(&file)?
		},
		(Some(_), Some(_)) => {
			eprintln!("config error: both --file and --config cannot be provided, exiting");
			std::process::exit(1);
		},
		(None, None) => {
			eprintln!("Error: either --file or --config must be provided, exiting");
			std::process::exit(1);
		},
	};

	let ct = tokio_util::sync::CancellationToken::new();
	let ct_clone = ct.clone();
	tokio::spawn(async move {
		let sig = signal::Shutdown::new();
		sig.wait().await;
		ct_clone.cancel();
	});

	match cfg.config_type {
		ConfigType::Static(r#static) => {
			let mut run_set = JoinSet::new();

			let cfg_clone = r#static.clone();

			let (update_tx, update_rx) = tokio::sync::mpsc::channel(100);
			let state = Arc::new(tokio::sync::RwLock::new(ProxyState::new(update_tx)));

			let ct_clone = ct.clone();
			let listener_manager = inbound::ListenerManager::new(
				ct_clone,
				state.clone(),
				update_rx,
				Arc::new(relay::metrics::Metrics::new(&mut registry)),
				Arc::new(a2a::metrics::Metrics::new(&mut registry)),
			)
			.await;

			state
				.write()
				.await
				.listeners
				.insert(cfg_clone.listener)
				.await
				.expect("failed to insert listener");

			let state_2 = state.clone();
			let cfg_clone = r#static.clone();
			let ct_clone = ct.clone();
			run_set.spawn(async move {
				run_local_client(&cfg_clone, state_2, listener_manager, ct_clone)
					.await
					.map_err(|e| anyhow::anyhow!("error running local client: {:?}", e))
			});

			// Add metrics listener
			let ct_clone = ct.clone();
			run_set.spawn(async move {
				mtrcs::start(Arc::new(registry), ct_clone, cfg.metrics)
					.await
					.map_err(|e| anyhow::anyhow!("error serving metrics: {:?}", e))
			});

			// Add admin listener
			let ct_clone = ct.clone();
			run_set.spawn(async move {
				admin::start(state.clone(), ct_clone, cfg.admin)
					.await
					.map_err(|e| anyhow::anyhow!("error serving admin: {:?}", e))
			});

			if let Some(cfg) = cfg.tracing {
				let provider = trcng::init_tracer(cfg)?;
				let ct_clone = ct.clone();
				run_set.spawn(async move {
					ct_clone.cancelled().await;
					provider
						.shutdown()
						.map_err(|e| anyhow::anyhow!("error initializing tracer: {:?}", e))
				});
			}

			// Wait for all servers to finish? I think this does what I want :shrug:
			while let Some(result) = run_set.join_next().await {
				#[allow(unused_must_use)]
				result.unwrap();
			}
		},
		ConfigType::Xds(dynamic) => {
			let ct = tokio_util::sync::CancellationToken::new();
			let metrics = xds::metrics::Metrics::new(&mut registry);
			let awaiting_ready = tokio::sync::watch::channel(()).0;

			let (update_tx, update_rx) = tokio::sync::mpsc::channel(100);
			let state = Arc::new(tokio::sync::RwLock::new(ProxyState::new(update_tx)));

			let mut listener_manager = inbound::ListenerManager::new(
				ct.clone(),
				state.clone(),
				update_rx,
				Arc::new(relay::metrics::Metrics::new(&mut registry)),
				Arc::new(a2a::metrics::Metrics::new(&mut registry)),
			)
			.await;

			state
				.write()
				.await
				.listeners
				.insert(dynamic.listener.clone())
				.await
				.expect("failed to insert listener");

			let state_clone = state.clone();
			let updater = ProxyStateUpdater::new(state_clone);
			let cfg_clone = dynamic.clone();
			let xds_config = xds::client::Config::new(Arc::new(cfg_clone));
			let ads_client = xds_config
				.with_watched_handler::<XdsTarget>(xds::MCP_TARGET_TYPE, updater.clone())
				.with_watched_handler::<XdsListener>(xds::LISTENER_TYPE, updater)
				.build(metrics, awaiting_ready);

			let mut run_set = JoinSet::new();

			run_set.spawn(async move {
				ads_client
					.run()
					.await
					.map_err(|e| anyhow::anyhow!("error running xds client: {:?}", e))
			});

			// Add admin listener
			let ct_clone = ct.clone();
			let state_3 = state.clone();
			run_set.spawn(async move {
				admin::start(state_3, ct_clone, cfg.admin)
					.await
					.map_err(|e| anyhow::anyhow!("error serving admin: {:?}", e))
			});

			let ct_clone = ct.clone();
			run_set.spawn(async move {
				listener_manager
					.run(ct_clone)
					.await
					.map_err(|e| anyhow::anyhow!("error serving static listener: {:?}", e))
			});

			// Add metrics listener
			let ct_clone = ct.clone();
			run_set.spawn(async move {
				mtrcs::start(Arc::new(registry), ct_clone, cfg.metrics)
					.await
					.map_err(|e| anyhow::anyhow!("error serving metrics: {:?}", e))
			});

			if let Some(cfg) = cfg.tracing {
				let provider = trcng::init_tracer(cfg)?;
				let ct_clone = ct.clone();
				run_set.spawn(async move {
					ct_clone.cancelled().await;
					provider
						.shutdown()
						.map_err(|e| anyhow::anyhow!("error initializing tracer: {:?}", e))
				});
			}

			// Wait for all servers to finish? I think this does what I want :shrug:
			while let Some(result) = run_set.join_next().await {
				#[allow(unused_must_use)]
				result.unwrap();
			}
		},
	};

	Ok(())
}
