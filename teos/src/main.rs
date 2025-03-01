use log::LevelFilter;
use simple_logger::SimpleLogger;
use std::fs;
use std::io::ErrorKind;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;
use std::sync::{Arc, Condvar, Mutex};
use structopt::StructOpt;
use tokio::task;
use tonic::transport::{Certificate, Server, ServerTlsConfig};

use bitcoin::network::constants::Network;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use bitcoincore_rpc::{Auth, Client};
use lightning_block_sync::init::validate_best_block_header;
use lightning_block_sync::poll::{
    ChainPoller, Poll, Validate, ValidatedBlock, ValidatedBlockHeader,
};
use lightning_block_sync::{BlockSource, SpvClient, UnboundedCache};

use teos::api::internal::InternalAPI;
use teos::api::{http, tor};
use teos::bitcoin_cli::BitcoindClient;
use teos::carrier::Carrier;
use teos::chain_monitor::ChainMonitor;
use teos::config::{self, Config, Opt};
use teos::dbm::DBM;
use teos::gatekeeper::Gatekeeper;
use teos::protos::private_tower_services_server::PrivateTowerServicesServer;
use teos::protos::public_tower_services_server::PublicTowerServicesServer;
use teos::responder::Responder;
use teos::tls::tls_init;
use teos::watcher::Watcher;

use teos_common::constants::IRREVOCABLY_RESOLVED;
use teos_common::cryptography::get_random_keypair;
use teos_common::TowerId;

async fn get_last_n_blocks<B, T>(
    poller: &mut ChainPoller<B, T>,
    mut last_known_block: ValidatedBlockHeader,
    n: usize,
) -> Vec<ValidatedBlock>
where
    B: DerefMut<Target = T> + Sized + Send + Sync,
    T: BlockSource,
{
    let mut last_n_blocks = Vec::with_capacity(n);
    for _ in 0..n {
        let block = poller.fetch_block(&last_known_block).await.unwrap();
        last_known_block = poller
            .look_up_previous_header(&last_known_block)
            .await
            .unwrap();
        last_n_blocks.push(block);
    }

    last_n_blocks
}

fn create_new_tower_keypair(db: &DBM) -> (SecretKey, PublicKey) {
    let (sk, pk) = get_random_keypair();
    db.store_tower_key(&sk).unwrap();
    (sk, pk)
}

#[tokio::main]
async fn main() {
    let opt = Opt::from_args();
    let path = config::data_dir_absolute_path(opt.data_dir.clone());

    // Create data dir if it does not exist
    fs::create_dir_all(&path).unwrap_or_else(|e| {
        eprintln!("Cannot create data dir: {:?}", e);
        std::process::exit(1);
    });

    // Load conf (from file or defaults) and patch it with the command line parameters received (if any)
    let mut conf = config::from_file::<Config>(path.join("teos.toml"));
    let is_default = conf.is_default();
    conf.patch_with_options(opt);
    conf.verify().unwrap_or_else(|e| {
        eprintln!("{}", e);
        std::process::exit(1);
    });

    // Set log level
    SimpleLogger::new()
        .with_level(if conf.deps_debug {
            LevelFilter::Debug
        } else {
            LevelFilter::Warn
        })
        .with_module_level(
            "teos",
            if conf.debug {
                LevelFilter::Debug
            } else {
                LevelFilter::Info
            },
        )
        .init()
        .unwrap();

    if is_default {
        log::info!("Loading default configuration")
    } else {
        log::info!("Loading configuration from file")
    }

    // Create network dir
    let path_network = path.join(conf.btc_network.clone());
    fs::create_dir_all(&path_network).unwrap_or_else(|e| {
        eprintln!("Cannot create network dir: {:?}", e);
        std::process::exit(1);
    });
    let dbm = Arc::new(Mutex::new(
        DBM::new(path_network.join("teos_db.sql3")).unwrap(),
    ));

    // Load tower secret key or create a fresh one if none is found. If overwrite key is set, create a new
    // key straightaway
    let (tower_sk, tower_pk) = {
        let locked_db = dbm.lock().unwrap();
        if conf.overwrite_key {
            log::info!("Overwriting tower keys");
            create_new_tower_keypair(&locked_db)
        } else {
            match locked_db.load_tower_key() {
                Ok(sk) => (sk, PublicKey::from_secret_key(&Secp256k1::new(), &sk)),
                Err(_) => {
                    log::info!("Tower keys not found. Creating a fresh set");
                    create_new_tower_keypair(&locked_db)
                }
            }
        }
    };
    log::info!("tower_id: {}", tower_pk);

    // Initialize our bitcoind client
    let (bitcoin_cli, bitcoind_reachable) = match BitcoindClient::new(
        &conf.btc_rpc_connect,
        conf.btc_rpc_port,
        &conf.btc_rpc_user,
        &conf.btc_rpc_password,
        &conf.btc_network,
    )
    .await
    {
        Ok(client) => (
            Arc::new(client),
            Arc::new((Mutex::new(true), Condvar::new())),
        ),
        Err(e) => {
            let e_msg = match e.kind() {
                ErrorKind::InvalidData => "invalid btcrpcuser or btcrpcpassword".into(),
                _ => e.to_string(),
            };
            log::error!("Failed to connect to bitcoind. Error: {}", e_msg);
            return;
        }
    };

    // FIXME: Temporary. We're using bitcoin_core_rpc and rust-lightning's rpc until they both get merged
    // https://github.com/rust-bitcoin/rust-bitcoincore-rpc/issues/166
    let schema = if !conf.btc_rpc_connect.starts_with("http") {
        "http://"
    } else {
        ""
    };
    let rpc = Arc::new(
        Client::new(
            &format!("{}{}:{}", schema, conf.btc_rpc_connect, conf.btc_rpc_port),
            Auth::UserPass(conf.btc_rpc_user.clone(), conf.btc_rpc_password.clone()),
        )
        .unwrap(),
    );
    let mut derefed = bitcoin_cli.deref();
    // Load last known block from DB if found. Poll it from Bitcoind otherwise.
    let last_known_block = dbm.lock().unwrap().load_last_known_block();
    let tip = if let Ok(block_hash) = last_known_block {
        derefed
            .get_header(&block_hash, None)
            .await
            .unwrap()
            .validate(block_hash)
            .unwrap()
    } else {
        validate_best_block_header(&mut derefed).await.unwrap()
    };

    // DISCUSS: This is not really required (and only triggered in regtest). This is only in place so the caches can be
    // populated with enough blocks mainly because the size of the cache is based on the amount of blocks passed when initializing.
    // However, we could add an additional parameter to specify the size of the cache, and initialize with however may blocks we
    // could pull from the backend. Adding this functionality just for regtest seemed unnecessary though, hence the check.
    if tip.height < IRREVOCABLY_RESOLVED {
        log::error!(
            "Not enough blocks to start teosd (required: {}). Mine at least {} more",
            IRREVOCABLY_RESOLVED,
            IRREVOCABLY_RESOLVED - tip.height
        );
        std::process::exit(1);
    }

    log::info!("Last known block: {}", tip.header.block_hash());

    // This is how chain poller names bitcoin networks.
    let btc_network = match conf.btc_network.as_str() {
        "main" => "bitcoin",
        "test" => "testnet",
        any => any,
    };

    let mut poller = ChainPoller::new(&mut derefed, Network::from_str(btc_network).unwrap());
    let last_n_blocks = get_last_n_blocks(&mut poller, tip, IRREVOCABLY_RESOLVED as usize).await;

    // Build components
    let gatekeeper = Arc::new(Gatekeeper::new(
        tip.height,
        conf.subscription_slots,
        conf.subscription_duration,
        conf.expiry_delta,
        dbm.clone(),
    ));

    let carrier = Carrier::new(rpc, bitcoind_reachable.clone(), tip.height);
    let responder = Arc::new(Responder::new(
        &last_n_blocks,
        tip.height,
        carrier,
        gatekeeper.clone(),
        dbm.clone(),
    ));
    let watcher = Arc::new(Watcher::new(
        gatekeeper.clone(),
        responder.clone(),
        &last_n_blocks[0..6],
        tip.height,
        tower_sk,
        TowerId(tower_pk),
        dbm.clone(),
    ));

    if watcher.is_fresh() & responder.is_fresh() & gatekeeper.is_fresh() {
        log::info!("Fresh bootstrap");
    } else {
        log::info!("Bootstrapping from backed up data");
    }

    let (shutdown_trigger, shutdown_signal_rpc_api) = triggered::trigger();
    let shutdown_signal_internal_rpc_api = shutdown_signal_rpc_api.clone();
    let shutdown_signal_http = shutdown_signal_rpc_api.clone();
    let shutdown_signal_cm = shutdown_signal_rpc_api.clone();
    let shutdown_signal_tor = shutdown_signal_rpc_api.clone();

    // The ordering here actually matters. Listeners are called by order, and we want the gatekeeper to be called
    // last, so both the Watcher and the Responder can query the necessary data from it during data deletion.
    let listener = &(watcher.clone(), &(responder, gatekeeper));
    let cache = &mut UnboundedCache::new();
    let spv_client = SpvClient::new(tip, poller, cache, listener);
    let mut chain_monitor = ChainMonitor::new(
        spv_client,
        tip,
        dbm,
        conf.polling_delta,
        shutdown_signal_cm,
        bitcoind_reachable.clone(),
    )
    .await;

    // Get all the components up to date if there's a backlog of blocks
    chain_monitor.poll_best_tip().await;
    log::info!("Bootstrap completed. Turning on interfaces");

    // Build interfaces
    let rpc_api = Arc::new(InternalAPI::new(
        watcher,
        bitcoind_reachable.clone(),
        shutdown_trigger,
    ));
    let internal_rpc_api = rpc_api.clone();

    let rpc_api_addr = format!("{}:{}", conf.rpc_bind, conf.rpc_port)
        .parse()
        .unwrap();
    let internal_rpc_api_addr = format!("{}:{}", conf.internal_api_bind, conf.internal_api_port)
        .parse()
        .unwrap();
    let internal_rpc_api_uri = format!(
        "http://{}:{}",
        conf.internal_api_bind, conf.internal_api_port
    );
    let http_api_addr = format!("{}:{}", conf.api_bind, conf.api_port)
        .parse()
        .unwrap();

    // Generate mtls certificates to data directory so the admin can securely connect
    // to the server to perform administrative tasks.
    let (identity, ca_cert) = tls_init(&path).unwrap_or_else(|e| {
        eprintln!("Couldn't generate tls certificates: {:?}", e);
        std::process::exit(1);
    });

    let tls = ServerTlsConfig::new()
        .identity(identity)
        .client_ca_root(Certificate::from_pem(ca_cert));

    // Start tasks
    let private_api_task = task::spawn(async move {
        Server::builder()
            .tls_config(tls)
            .expect("couldn't configure tls")
            .add_service(PrivateTowerServicesServer::new(rpc_api))
            .serve_with_shutdown(rpc_api_addr, shutdown_signal_rpc_api)
            .await
            .unwrap();
    });

    let public_api_task = task::spawn(async move {
        Server::builder()
            .add_service(PublicTowerServicesServer::new(internal_rpc_api))
            .serve_with_shutdown(internal_rpc_api_addr, shutdown_signal_internal_rpc_api)
            .await
            .unwrap();
    });

    let (http_service_ready, ready_signal_http) = triggered::trigger();
    let http_api_task = task::spawn(http::serve(
        http_api_addr,
        internal_rpc_api_uri,
        http_service_ready,
        shutdown_signal_http,
    ));
    ready_signal_http.await;

    // Add Tor Onion Service for public API
    let mut tor_task = Option::None;
    let (tor_service_ready, ready_signal_tor) = triggered::trigger();
    if conf.tor_support {
        log::info!("Starting up Tor hidden service");
        let tor_control_port = conf.tor_control_port;
        let onion_port = conf.onion_hidden_service_port;

        tor_task = Some(task::spawn(async move {
            if let Err(e) = tor::expose_onion_service(
                tor_control_port,
                http_api_addr,
                onion_port,
                path_network,
                tor_service_ready,
                shutdown_signal_tor,
            )
            .await
            {
                eprintln!("Cannot connect to the Tor backend: {}", e);
                std::process::exit(1);
            }
        }));

        ready_signal_tor.await
    }

    log::info!("Tower ready");
    chain_monitor.monitor_chain().await;

    // Wait until shutdown
    http_api_task.await.unwrap();
    private_api_task.await.unwrap();
    public_api_task.await.unwrap();
    if conf.tor_support {
        tor_task.unwrap().await.unwrap();
    }

    log::info!("Shutting down tower");
}
