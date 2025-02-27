#![allow(clippy::upper_case_acronyms, clippy::derive_partial_eq_without_eq)]

use std::{str::FromStr, sync::Arc, time::Instant};

use api_server::execution_sandbox::VmConcurrencyLimiter;
use futures::channel::oneshot;
use tokio::{sync::watch, task::JoinHandle};

use house_keeper::periodic_job::PeriodicJob;
use prometheus_exporter::run_prometheus_exporter;
use zksync_circuit_breaker::{
    facet_selectors::FacetSelectorsChecker, l1_txs::FailedL1TransactionChecker, vks::VksChecker,
    CircuitBreaker, CircuitBreakerChecker, CircuitBreakerError,
};
use zksync_config::configs::{
    api::{HealthCheckConfig, Web3JsonRpcConfig},
    chain::{
        self, CircuitBreakerConfig, MempoolConfig, NetworkConfig, OperationsManagerConfig,
        StateKeeperConfig,
    },
    house_keeper::HouseKeeperConfig,
    FriProverConfig, FriWitnessGeneratorConfig, PrometheusConfig, ProverGroupConfig,
    WitnessGeneratorConfig,
};
use zksync_config::{
    ApiConfig, ContractsConfig, DBConfig, ETHClientConfig, ETHSenderConfig, FetcherConfig,
    ProverConfigs,
};
use zksync_contracts::BaseSystemContractsHashes;
use zksync_dal::{
    connection::DbVariant, healthcheck::ConnectionPoolHealthCheck, ConnectionPool, StorageProcessor,
};
use zksync_eth_client::clients::http::QueryClient;
use zksync_eth_client::{clients::http::PKSigningClient, BoundEthInterface};
use zksync_health_check::CheckHealth;
use zksync_object_store::ObjectStoreFactory;
use zksync_queued_job_processor::JobProcessor;
use zksync_state::FactoryDepsCache;
use zksync_types::{proofs::AggregationRound, L2ChainId, PackedEthSignature, H160};

use crate::api_server::healthcheck::HealthCheckHandle;
use crate::api_server::tx_sender::TxSenderConfig;
use crate::api_server::web3::api_health_check::ApiHealthCheck;
use crate::api_server::web3::state::InternalApiConfig;
use crate::api_server::{
    healthcheck,
    tx_sender::{TxSender, TxSenderBuilder},
};
use crate::eth_sender::{Aggregator, EthTxManager};
use crate::house_keeper::fri_prover_job_retry_manager::FriProverJobRetryManager;
use crate::house_keeper::fri_prover_queue_monitor::FriProverStatsReporter;
use crate::house_keeper::fri_scheduler_circuit_queuer::SchedulerCircuitQueuer;
use crate::house_keeper::fri_witness_generator_jobs_retry_manager::FriWitnessGeneratorJobRetryManager;
use crate::house_keeper::fri_witness_generator_queue_monitor::FriWitnessGeneratorStatsReporter;
use crate::house_keeper::gcs_blob_cleaner::GcsBlobCleaner;
use crate::house_keeper::{
    blocks_state_reporter::L1BatchMetricsReporter, gpu_prover_queue_monitor::GpuProverQueueMonitor,
    prover_job_retry_manager::ProverJobRetryManager, prover_queue_monitor::ProverStatsReporter,
    waiting_to_queued_fri_witness_job_mover::WaitingToQueuedFriWitnessJobMover,
    waiting_to_queued_witness_job_mover::WaitingToQueuedWitnessJobMover,
    witness_generator_queue_monitor::WitnessGeneratorStatsReporter,
};
use crate::l1_gas_price::{GasAdjusterSingleton, L1GasPriceProvider};
use crate::metadata_calculator::{
    MetadataCalculator, MetadataCalculatorConfig, MetadataCalculatorModeConfig, TreeHealthCheck,
};
use crate::state_keeper::{create_state_keeper, MempoolFetcher, MempoolGuard, MiniblockSealer};
use crate::witness_generator::{
    basic_circuits::BasicWitnessGenerator, leaf_aggregation::LeafAggregationWitnessGenerator,
    node_aggregation::NodeAggregationWitnessGenerator, scheduler::SchedulerWitnessGenerator,
};
use crate::{
    api_server::{explorer, web3},
    data_fetchers::run_data_fetchers,
    eth_sender::EthTxAggregator,
    eth_watch::start_eth_watch,
};

pub mod api_server;
pub mod block_reverter;
pub mod consistency_checker;
pub mod data_fetchers;
pub mod eth_sender;
pub mod eth_watch;
pub mod fee_ticker;
pub mod gas_tracker;
pub mod genesis;
pub mod house_keeper;
pub mod l1_gas_price;
pub mod metadata_calculator;
pub mod reorg_detector;
pub mod state_keeper;
pub mod sync_layer;
pub mod witness_generator;

/// Inserts the initial information about zkSync tokens into the database.
pub async fn genesis_init(eth_sender: &ETHSenderConfig, network_config: &NetworkConfig) {
    let mut storage = StorageProcessor::establish_connection(true).await;
    let operator_address = PackedEthSignature::address_from_private_key(
        &eth_sender
            .sender
            .private_key()
            .expect("Private key is required for genesis init"),
    )
    .expect("Failed to restore operator address from private key");

    genesis::ensure_genesis_state(
        &mut storage,
        L2ChainId(network_config.zksync_network_id),
        &genesis::GenesisParams::MainNode {
            // We consider the operator to be the first validator for now.
            first_validator: operator_address,
        },
    )
    .await;
}

pub async fn is_genesis_needed() -> bool {
    let mut storage = StorageProcessor::establish_connection(true).await;
    storage.blocks_dal().is_genesis_needed().await
}

/// Sets up an interrupt handler and returns a future that resolves once an interrupt signal
/// is received.
pub fn setup_sigint_handler() -> oneshot::Receiver<()> {
    let (sigint_sender, sigint_receiver) = oneshot::channel();
    let mut sigint_sender = Some(sigint_sender);
    ctrlc::set_handler(move || {
        if let Some(sigint_sender) = sigint_sender.take() {
            sigint_sender.send(()).ok();
            // ^ The send fails if `sigint_receiver` is dropped. We're OK with this,
            // since at this point the node should be stopping anyway, or is not interested
            // in listening to interrupt signals.
        }
    })
    .expect("Error setting Ctrl+C handler");

    sigint_receiver
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Component {
    // Public Web3 API running on HTTP server.
    HttpApi,
    // Public Web3 API (including PubSub) running on WebSocket server.
    WsApi,
    // REST API for explorer.
    ExplorerApi,
    // Metadata Calculator.
    Tree,
    TreeLightweight,
    TreeBackup,
    EthWatcher,
    // Eth tx generator
    EthTxAggregator,
    // Manager for eth tx
    EthTxManager,
    // Data fetchers: list fetcher, volume fetcher, price fetcher.
    DataFetcher,
    // State keeper.
    StateKeeper,
    // Witness Generator. The first argument is a number of jobs to process. If None, runs indefinitely.
    // The second argument is the type of the witness-generation performed
    WitnessGenerator(Option<usize>, AggregationRound),
    // Component for housekeeping task such as cleaning blobs from GCS, reporting metrics etc.
    Housekeeper,
}

#[derive(Debug)]
pub struct Components(pub Vec<Component>);

impl FromStr for Components {
    type Err = String;

    fn from_str(s: &str) -> Result<Components, String> {
        match s {
            "api" => Ok(Components(vec![
                Component::HttpApi,
                Component::WsApi,
                Component::ExplorerApi,
            ])),
            "http_api" => Ok(Components(vec![Component::HttpApi])),
            "ws_api" => Ok(Components(vec![Component::WsApi])),
            "explorer_api" => Ok(Components(vec![Component::ExplorerApi])),
            "tree" | "tree_new" => Ok(Components(vec![Component::Tree])),
            "tree_lightweight" | "tree_lightweight_new" => {
                Ok(Components(vec![Component::TreeLightweight]))
            }
            "tree_backup" => Ok(Components(vec![Component::TreeBackup])),
            "data_fetcher" => Ok(Components(vec![Component::DataFetcher])),
            "state_keeper" => Ok(Components(vec![Component::StateKeeper])),
            "housekeeper" => Ok(Components(vec![Component::Housekeeper])),
            "witness_generator" => Ok(Components(vec![
                Component::WitnessGenerator(None, AggregationRound::BasicCircuits),
                Component::WitnessGenerator(None, AggregationRound::LeafAggregation),
                Component::WitnessGenerator(None, AggregationRound::NodeAggregation),
                Component::WitnessGenerator(None, AggregationRound::Scheduler),
            ])),
            "one_shot_witness_generator" => Ok(Components(vec![
                Component::WitnessGenerator(Some(1), AggregationRound::BasicCircuits),
                Component::WitnessGenerator(Some(1), AggregationRound::LeafAggregation),
                Component::WitnessGenerator(Some(1), AggregationRound::NodeAggregation),
                Component::WitnessGenerator(Some(1), AggregationRound::Scheduler),
            ])),
            "one_shot_basic_witness_generator" => {
                Ok(Components(vec![Component::WitnessGenerator(
                    Some(1),
                    AggregationRound::BasicCircuits,
                )]))
            }
            "one_shot_leaf_witness_generator" => Ok(Components(vec![Component::WitnessGenerator(
                Some(1),
                AggregationRound::LeafAggregation,
            )])),
            "one_shot_node_witness_generator" => Ok(Components(vec![Component::WitnessGenerator(
                Some(1),
                AggregationRound::NodeAggregation,
            )])),
            "one_shot_scheduler_witness_generator" => {
                Ok(Components(vec![Component::WitnessGenerator(
                    Some(1),
                    AggregationRound::Scheduler,
                )]))
            }
            "eth" => Ok(Components(vec![
                Component::EthWatcher,
                Component::EthTxAggregator,
                Component::EthTxManager,
            ])),
            "eth_watcher" => Ok(Components(vec![Component::EthWatcher])),
            "eth_tx_aggregator" => Ok(Components(vec![Component::EthTxAggregator])),
            "eth_tx_manager" => Ok(Components(vec![Component::EthTxManager])),
            other => Err(format!("{} is not a valid component name", other)),
        }
    }
}

pub async fn initialize_components(
    components: Vec<Component>,
    use_prometheus_pushgateway: bool,
) -> anyhow::Result<(
    Vec<JoinHandle<()>>,
    watch::Sender<bool>,
    oneshot::Receiver<CircuitBreakerError>,
    HealthCheckHandle,
)> {
    vlog::info!("Starting the components: {components:?}");
    let connection_pool = ConnectionPool::new(None, DbVariant::Master).await;
    let prover_connection_pool = ConnectionPool::new(None, DbVariant::Prover).await;
    let replica_connection_pool = ConnectionPool::new(None, DbVariant::Replica).await;
    let mut healthchecks: Vec<Box<dyn CheckHealth>> = Vec::new();
    let contracts_config = ContractsConfig::from_env();
    let eth_client_config = ETHClientConfig::from_env();
    let circuit_breaker_config = CircuitBreakerConfig::from_env();
    let circuit_breaker_checker = CircuitBreakerChecker::new(
        circuit_breakers_for_components(
            &components,
            &eth_client_config.web3_url,
            &circuit_breaker_config,
            contracts_config.diamond_proxy_addr,
        )
        .await,
        &circuit_breaker_config,
    );
    circuit_breaker_checker.check().await.unwrap_or_else(|err| {
        panic!("Circuit breaker triggered: {}", err);
    });

    let query_client = QueryClient::new(&eth_client_config.web3_url).unwrap();
    let mut gas_adjuster = GasAdjusterSingleton::new();

    let (stop_sender, stop_receiver) = watch::channel(false);
    let (cb_sender, cb_receiver) = oneshot::channel();
    // Prometheus exporter and circuit breaker checker should run for every component configuration.
    let prom_config = PrometheusConfig::from_env();
    let mut task_futures: Vec<JoinHandle<()>> = vec![
        run_prometheus_exporter(
            prom_config.listener_port,
            use_prometheus_pushgateway.then(|| {
                (
                    prom_config.pushgateway_url.clone(),
                    prom_config.push_interval(),
                )
            }),
        ),
        tokio::spawn(circuit_breaker_checker.run(cb_sender, stop_receiver.clone())),
    ];

    let factory_deps_cache = FactoryDepsCache::new(
        "factory_deps_cache",
        Web3JsonRpcConfig::from_env().factory_deps_cache_size_mb(),
    );

    if components.contains(&Component::WsApi)
        || components.contains(&Component::HttpApi)
        || components.contains(&Component::ExplorerApi)
    {
        let api_config = ApiConfig::from_env();
        let state_keeper_config = StateKeeperConfig::from_env();
        let network_config = NetworkConfig::from_env();
        let tx_sender_config = TxSenderConfig::new(&state_keeper_config, &api_config.web3_json_rpc);
        let internal_api_config = InternalApiConfig::new(
            &network_config,
            &api_config.web3_json_rpc,
            &contracts_config,
        );
        if components.contains(&Component::HttpApi) {
            let started_at = Instant::now();
            vlog::info!("initializing HTTP API");
            let bounded_gas_adjuster = gas_adjuster.get_or_init_bounded().await;
            let (futures, health_check) = run_http_api(
                &tx_sender_config,
                &state_keeper_config,
                &internal_api_config,
                &api_config,
                connection_pool.clone(),
                replica_connection_pool.clone(),
                stop_receiver.clone(),
                bounded_gas_adjuster.clone(),
                state_keeper_config.save_call_traces,
                factory_deps_cache.clone(),
            )
            .await;
            task_futures.extend(futures);
            healthchecks.push(Box::new(health_check));
            vlog::info!("initialized HTTP API in {:?}", started_at.elapsed());
            metrics::gauge!("server.init.latency", started_at.elapsed(), "stage" => "http_api");
        }

        if components.contains(&Component::WsApi) {
            let started_at = Instant::now();
            vlog::info!("initializing WS API");
            let bounded_gas_adjuster = gas_adjuster.get_or_init_bounded().await;
            let (futures, health_check) = run_ws_api(
                &tx_sender_config,
                &state_keeper_config,
                &internal_api_config,
                &api_config,
                bounded_gas_adjuster.clone(),
                connection_pool.clone(),
                replica_connection_pool.clone(),
                stop_receiver.clone(),
                factory_deps_cache.clone(),
            )
            .await;
            task_futures.extend(futures);
            healthchecks.push(Box::new(health_check));
            vlog::info!("initialized WS API in {:?}", started_at.elapsed());
            metrics::gauge!("server.init.latency", started_at.elapsed(), "stage" => "ws_api");
        }

        if components.contains(&Component::ExplorerApi) {
            let started_at = Instant::now();
            vlog::info!("initializing explorer REST API");
            task_futures.push(explorer::start_server_thread_detached(
                api_config.explorer.clone(),
                contracts_config.l2_erc20_bridge_addr,
                state_keeper_config.fee_account_addr,
                connection_pool.clone(),
                replica_connection_pool.clone(),
                stop_receiver.clone(),
            ));
            vlog::info!(
                "initialized explorer REST API in {:?}",
                started_at.elapsed()
            );
            metrics::gauge!("server.init.latency", started_at.elapsed(), "stage" => "explorer_api");
        }
    }

    if components.contains(&Component::StateKeeper) {
        let started_at = Instant::now();
        vlog::info!("initializing State Keeper");
        let bounded_gas_adjuster = gas_adjuster.get_or_init_bounded().await;
        add_state_keeper_to_task_futures(
            &mut task_futures,
            &contracts_config,
            StateKeeperConfig::from_env(),
            &DBConfig::from_env(),
            &MempoolConfig::from_env(),
            bounded_gas_adjuster,
            stop_receiver.clone(),
        )
        .await;
        vlog::info!("initialized State Keeper in {:?}", started_at.elapsed());
        metrics::gauge!("server.init.latency", started_at.elapsed(), "stage" => "state_keeper");
    }

    if components.contains(&Component::EthWatcher) {
        let started_at = Instant::now();
        vlog::info!("initializing ETH-Watcher");
        let eth_watch_pool = ConnectionPool::new(Some(1), DbVariant::Master).await;
        task_futures.push(
            start_eth_watch(
                eth_watch_pool,
                query_client.clone(),
                contracts_config.diamond_proxy_addr,
                stop_receiver.clone(),
            )
            .await,
        );
        vlog::info!("initialized ETH-Watcher in {:?}", started_at.elapsed());
        metrics::gauge!("server.init.latency", started_at.elapsed(), "stage" => "eth_watcher");
    }

    if components.contains(&Component::EthTxAggregator) {
        let started_at = Instant::now();
        vlog::info!("initializing ETH-TxAggregator");
        let eth_sender_storage = ConnectionPool::new(Some(1), DbVariant::Master).await;
        let eth_sender_prover_storage = ConnectionPool::new(Some(1), DbVariant::Prover).await;

        let eth_sender = ETHSenderConfig::from_env();
        let eth_client =
            PKSigningClient::from_config(&eth_sender, &contracts_config, &eth_client_config);
        let nonce = eth_client.pending_nonce("eth_sender").await.unwrap();
        let eth_tx_aggregator_actor = EthTxAggregator::new(
            eth_sender.sender.clone(),
            Aggregator::new(eth_sender.sender.clone()),
            contracts_config.validator_timelock_addr,
            nonce.as_u64(),
        );
        task_futures.push(tokio::spawn(eth_tx_aggregator_actor.run(
            eth_sender_storage.clone(),
            eth_sender_prover_storage.clone(),
            eth_client,
            stop_receiver.clone(),
        )));
        vlog::info!("initialized ETH-TxAggregator in {:?}", started_at.elapsed());
        metrics::gauge!("server.init.latency", started_at.elapsed(), "stage" => "eth_tx_aggregator");
    }

    if components.contains(&Component::EthTxManager) {
        let started_at = Instant::now();
        vlog::info!("initializing ETH-TxManager");
        let eth_sender_storage = ConnectionPool::new(Some(1), DbVariant::Master).await;
        let eth_sender = ETHSenderConfig::from_env();
        let eth_client =
            PKSigningClient::from_config(&eth_sender, &contracts_config, &eth_client_config);
        let eth_tx_manager_actor = EthTxManager::new(
            eth_sender.sender,
            gas_adjuster.get_or_init().await,
            eth_client,
        );
        task_futures.extend([tokio::spawn(
            eth_tx_manager_actor.run(eth_sender_storage, stop_receiver.clone()),
        )]);
        vlog::info!("initialized ETH-TxManager in {:?}", started_at.elapsed());
        metrics::gauge!("server.init.latency", started_at.elapsed(), "stage" => "eth_tx_aggregator");
    }

    if components.contains(&Component::DataFetcher) {
        let started_at = Instant::now();
        let fetcher_config = FetcherConfig::from_env();
        let eth_network = chain::NetworkConfig::from_env();
        vlog::info!("initializing data fetchers");
        task_futures.extend(run_data_fetchers(
            &fetcher_config,
            eth_network.network,
            connection_pool.clone(),
            stop_receiver.clone(),
        ));
        vlog::info!("initialized data fetchers in {:?}", started_at.elapsed());
        metrics::gauge!("server.init.latency", started_at.elapsed(), "stage" => "data_fetchers");
    }

    let store_factory = ObjectStoreFactory::from_env();
    add_trees_to_task_futures(
        &mut task_futures,
        &mut healthchecks,
        &components,
        &store_factory,
        &stop_receiver,
    )
    .await;
    add_witness_generator_to_task_futures(
        &mut task_futures,
        &components,
        &connection_pool,
        &prover_connection_pool,
        &store_factory,
        &stop_receiver,
    )
    .await;

    if components.contains(&Component::Housekeeper) {
        add_house_keeper_to_task_futures(&mut task_futures, &store_factory).await;
    }

    // Run healthcheck server for all components.
    healthchecks.push(Box::new(ConnectionPoolHealthCheck::new(
        replica_connection_pool,
    )));

    let healtcheck_api_config = HealthCheckConfig::from_env();
    let health_check_handle =
        healthcheck::start_server_thread_detached(healtcheck_api_config.bind_addr(), healthchecks);

    if let Some(task) = gas_adjuster.run_if_initialized(stop_receiver.clone()) {
        task_futures.push(task);
    }
    Ok((task_futures, stop_sender, cb_receiver, health_check_handle))
}

async fn add_state_keeper_to_task_futures<E: L1GasPriceProvider + Send + Sync + 'static>(
    task_futures: &mut Vec<JoinHandle<()>>,
    contracts_config: &ContractsConfig,
    state_keeper_config: StateKeeperConfig,
    db_config: &DBConfig,
    mempool_config: &MempoolConfig,
    gas_adjuster: Arc<E>,
    stop_receiver: watch::Receiver<bool>,
) {
    let fair_l2_gas_price = state_keeper_config.fair_l2_gas_price;
    let state_keeper_pool = ConnectionPool::new(Some(1), DbVariant::Master).await;
    let next_priority_id = state_keeper_pool
        .access_storage()
        .await
        .transactions_dal()
        .next_priority_id()
        .await;
    let mempool = MempoolGuard::new(next_priority_id, mempool_config.capacity);

    let miniblock_sealer_pool = ConnectionPool::new(Some(1), DbVariant::Master).await;
    let (miniblock_sealer, miniblock_sealer_handle) = MiniblockSealer::new(
        miniblock_sealer_pool,
        state_keeper_config.miniblock_seal_queue_capacity,
    );
    task_futures.push(tokio::spawn(miniblock_sealer.run()));

    let state_keeper = create_state_keeper(
        contracts_config,
        state_keeper_config,
        db_config,
        mempool_config,
        state_keeper_pool,
        mempool.clone(),
        gas_adjuster.clone(),
        miniblock_sealer_handle,
        stop_receiver.clone(),
    )
    .await;
    task_futures.push(tokio::spawn(state_keeper.run()));

    let mempool_fetcher_pool = ConnectionPool::new(Some(1), DbVariant::Master).await;
    let mempool_fetcher = MempoolFetcher::new(mempool, gas_adjuster, mempool_config);
    let mempool_fetcher_handle = tokio::spawn(mempool_fetcher.run(
        mempool_fetcher_pool,
        mempool_config.remove_stuck_txs,
        mempool_config.stuck_tx_timeout(),
        fair_l2_gas_price,
        stop_receiver,
    ));
    task_futures.push(mempool_fetcher_handle);
}

async fn add_trees_to_task_futures(
    task_futures: &mut Vec<JoinHandle<()>>,
    healthchecks: &mut Vec<Box<dyn CheckHealth>>,
    components: &[Component],
    store_factory: &ObjectStoreFactory,
    stop_receiver: &watch::Receiver<bool>,
) {
    let db_config = DBConfig::from_env();
    let operation_config = OperationsManagerConfig::from_env();
    const COMPONENTS_TO_MODES: &[(Component, bool)] =
        &[(Component::Tree, true), (Component::TreeLightweight, false)];

    if components.contains(&Component::TreeBackup) {
        panic!("Tree backup mode is disabled");
    }
    if components.contains(&Component::Tree) && components.contains(&Component::TreeLightweight) {
        panic!(
            "Cannot start a node with a Merkle tree in both full and lightweight modes. \
             Since the storage layout is mode-independent, choose either of modes and run \
             the node with it."
        );
    }

    for &(component, is_full) in COMPONENTS_TO_MODES {
        if components.contains(&component) {
            let mode = if is_full {
                MetadataCalculatorModeConfig::Full { store_factory }
            } else {
                MetadataCalculatorModeConfig::Lightweight
            };
            let (future, tree_health_check) =
                run_tree(&db_config, &operation_config, mode, stop_receiver.clone()).await;
            task_futures.push(future);
            healthchecks.push(Box::new(tree_health_check));
        }
    }
}

async fn run_tree(
    config: &DBConfig,
    operation_manager: &OperationsManagerConfig,
    mode: MetadataCalculatorModeConfig<'_>,
    stop_receiver: watch::Receiver<bool>,
) -> (JoinHandle<()>, TreeHealthCheck) {
    let started_at = Instant::now();
    let mode_str = if matches!(mode, MetadataCalculatorModeConfig::Full { .. }) {
        "full"
    } else {
        "lightweight"
    };
    vlog::info!("Initializing Merkle tree in {mode_str} mode");

    let config = MetadataCalculatorConfig::for_main_node(config, operation_manager, mode);
    let metadata_calculator = MetadataCalculator::new(&config).await;
    let tree_health_check = metadata_calculator.tree_health_check();
    let tree_tag = metadata_calculator.tree_tag();
    let pool = ConnectionPool::new(Some(1), DbVariant::Master).await;
    let prover_pool = ConnectionPool::new(Some(1), DbVariant::Prover).await;
    let future = tokio::spawn(metadata_calculator.run(pool, prover_pool, stop_receiver));

    vlog::info!(
        "Initialized `{tree_tag}` tree in {:?}",
        started_at.elapsed()
    );
    metrics::gauge!(
        "server.init.latency",
        started_at.elapsed(),
        "stage" => "tree",
        "tree" => tree_tag
    );
    (future, tree_health_check)
}

async fn add_witness_generator_to_task_futures(
    task_futures: &mut Vec<JoinHandle<()>>,
    components: &[Component],
    connection_pool: &ConnectionPool,
    prover_connection_pool: &ConnectionPool,
    store_factory: &ObjectStoreFactory,
    stop_receiver: &watch::Receiver<bool>,
) {
    // We don't want witness generator to run on local nodes, as it's CPU heavy.
    if std::env::var("ZKSYNC_LOCAL_SETUP") == Ok("true".to_owned()) {
        return;
    }

    let generator_params = components.iter().filter_map(|component| {
        if let Component::WitnessGenerator(batch_size, component_type) = component {
            Some((*batch_size, *component_type))
        } else {
            None
        }
    });

    for (batch_size, component_type) in generator_params {
        let started_at = Instant::now();
        vlog::info!(
            "initializing the {component_type:?} witness generator, batch size: {batch_size:?}"
        );

        let config = WitnessGeneratorConfig::from_env();
        let task = match component_type {
            AggregationRound::BasicCircuits => {
                let witness_generator = BasicWitnessGenerator::new(
                    config,
                    store_factory,
                    connection_pool.clone(),
                    prover_connection_pool.clone(),
                )
                .await;
                tokio::spawn(witness_generator.run(stop_receiver.clone(), batch_size))
            }
            AggregationRound::LeafAggregation => {
                let witness_generator = LeafAggregationWitnessGenerator::new(
                    config,
                    store_factory,
                    connection_pool.clone(),
                    prover_connection_pool.clone(),
                )
                .await;
                tokio::spawn(witness_generator.run(stop_receiver.clone(), batch_size))
            }
            AggregationRound::NodeAggregation => {
                let witness_generator = NodeAggregationWitnessGenerator::new(
                    config,
                    store_factory,
                    connection_pool.clone(),
                    prover_connection_pool.clone(),
                )
                .await;
                tokio::spawn(witness_generator.run(stop_receiver.clone(), batch_size))
            }
            AggregationRound::Scheduler => {
                let witness_generator = SchedulerWitnessGenerator::new(
                    config,
                    store_factory,
                    connection_pool.clone(),
                    prover_connection_pool.clone(),
                )
                .await;
                tokio::spawn(witness_generator.run(stop_receiver.clone(), batch_size))
            }
        };
        task_futures.push(task);

        vlog::info!(
            "initialized {component_type:?} witness generator in {:?}",
            started_at.elapsed()
        );
        metrics::gauge!(
            "server.init.latency",
            started_at.elapsed(),
            "stage" => format!("witness_generator_{component_type:?}")
        );
    }
}

async fn add_house_keeper_to_task_futures(
    task_futures: &mut Vec<JoinHandle<()>>,
    store_factory: &ObjectStoreFactory,
) {
    let house_keeper_config = HouseKeeperConfig::from_env();
    let connection_pool = ConnectionPool::new(Some(1), DbVariant::Replica).await;
    let l1_batch_metrics_reporter = L1BatchMetricsReporter::new(
        house_keeper_config.l1_batch_metrics_reporting_interval_ms,
        connection_pool,
    );

    let prover_connection_pool = ConnectionPool::new(
        Some(house_keeper_config.prover_db_pool_size),
        DbVariant::Prover,
    )
    .await;
    let gpu_prover_queue = GpuProverQueueMonitor::new(
        ProverGroupConfig::from_env().synthesizer_per_gpu,
        house_keeper_config.gpu_prover_queue_reporting_interval_ms,
        prover_connection_pool.clone(),
    );
    let config = ProverConfigs::from_env().non_gpu;
    let prover_job_retry_manager = ProverJobRetryManager::new(
        config.max_attempts,
        config.proof_generation_timeout(),
        house_keeper_config.prover_job_retrying_interval_ms,
        prover_connection_pool.clone(),
    );
    let prover_stats_reporter = ProverStatsReporter::new(
        house_keeper_config.prover_stats_reporting_interval_ms,
        prover_connection_pool.clone(),
    );
    let waiting_to_queued_witness_job_mover = WaitingToQueuedWitnessJobMover::new(
        house_keeper_config.witness_job_moving_interval_ms,
        prover_connection_pool.clone(),
    );
    let witness_generator_stats_reporter = WitnessGeneratorStatsReporter::new(
        house_keeper_config.witness_generator_stats_reporting_interval_ms,
        prover_connection_pool.clone(),
    );
    let gcs_blob_cleaner = GcsBlobCleaner::new(
        store_factory,
        prover_connection_pool.clone(),
        house_keeper_config.blob_cleaning_interval_ms,
    )
    .await;

    task_futures.push(tokio::spawn(gcs_blob_cleaner.run()));
    task_futures.push(tokio::spawn(witness_generator_stats_reporter.run()));
    task_futures.push(tokio::spawn(gpu_prover_queue.run()));
    task_futures.push(tokio::spawn(l1_batch_metrics_reporter.run()));
    task_futures.push(tokio::spawn(prover_stats_reporter.run()));
    task_futures.push(tokio::spawn(waiting_to_queued_witness_job_mover.run()));
    task_futures.push(tokio::spawn(prover_job_retry_manager.run()));

    // All FRI Prover related components are configured below.
    let fri_prover_config = FriProverConfig::from_env();
    let fri_prover_job_retry_manager = FriProverJobRetryManager::new(
        fri_prover_config.max_attempts,
        fri_prover_config.proof_generation_timeout(),
        house_keeper_config.fri_prover_job_retrying_interval_ms,
        prover_connection_pool.clone(),
    );
    task_futures.push(tokio::spawn(fri_prover_job_retry_manager.run()));

    let fri_witness_gen_config = FriWitnessGeneratorConfig::from_env();
    let fri_witness_gen_job_retry_manager = FriWitnessGeneratorJobRetryManager::new(
        fri_witness_gen_config.max_attempts,
        fri_witness_gen_config.witness_generation_timeout(),
        house_keeper_config.fri_witness_generator_job_retrying_interval_ms,
        prover_connection_pool.clone(),
    );
    task_futures.push(tokio::spawn(fri_witness_gen_job_retry_manager.run()));

    let waiting_to_queued_fri_witness_job_mover = WaitingToQueuedFriWitnessJobMover::new(
        house_keeper_config.fri_witness_job_moving_interval_ms,
        prover_connection_pool.clone(),
    );
    task_futures.push(tokio::spawn(waiting_to_queued_fri_witness_job_mover.run()));

    let scheduler_circuit_queuer = SchedulerCircuitQueuer::new(
        house_keeper_config.fri_witness_job_moving_interval_ms,
        prover_connection_pool.clone(),
    );
    task_futures.push(tokio::spawn(scheduler_circuit_queuer.run()));

    let fri_witness_generator_stats_reporter = FriWitnessGeneratorStatsReporter::new(
        prover_connection_pool.clone(),
        house_keeper_config.witness_generator_stats_reporting_interval_ms,
    );
    task_futures.push(tokio::spawn(fri_witness_generator_stats_reporter.run()));

    let fri_prover_stats_reporter = FriProverStatsReporter::new(
        house_keeper_config.fri_prover_stats_reporting_interval_ms,
        prover_connection_pool.clone(),
    );
    task_futures.push(tokio::spawn(fri_prover_stats_reporter.run()));
}

async fn build_tx_sender<G: L1GasPriceProvider>(
    tx_sender_config: &TxSenderConfig,
    web3_json_config: &Web3JsonRpcConfig,
    state_keeper_config: &StateKeeperConfig,
    replica_pool: ConnectionPool,
    master_pool: ConnectionPool,
    l1_gas_price_provider: Arc<G>,
    factory_deps_cache: FactoryDepsCache,
) -> TxSender<G> {
    let mut tx_sender_builder = TxSenderBuilder::new(tx_sender_config.clone(), replica_pool)
        .with_main_connection_pool(master_pool)
        .with_state_keeper_config(state_keeper_config.clone());

    // Add rate limiter if enabled.
    if let Some(transactions_per_sec_limit) = web3_json_config.transactions_per_sec_limit {
        tx_sender_builder = tx_sender_builder.with_rate_limiter(transactions_per_sec_limit);
    };

    let vm_concurrency_limiter = VmConcurrencyLimiter::new(web3_json_config.vm_concurrency_limit);

    tx_sender_builder
        .build(
            l1_gas_price_provider,
            tx_sender_config.default_aa,
            Arc::new(vm_concurrency_limiter),
            factory_deps_cache,
        )
        .await
}

#[allow(clippy::too_many_arguments)]
async fn run_http_api<G: L1GasPriceProvider + Send + Sync + 'static>(
    tx_sender_config: &TxSenderConfig,
    state_keeper_config: &StateKeeperConfig,
    internal_api: &InternalApiConfig,
    api_config: &ApiConfig,
    master_connection_pool: ConnectionPool,
    replica_connection_pool: ConnectionPool,
    stop_receiver: watch::Receiver<bool>,
    gas_adjuster: Arc<G>,
    with_debug_namespace: bool,
    factory_deps_cache: FactoryDepsCache,
) -> (Vec<JoinHandle<()>>, ApiHealthCheck) {
    let tx_sender = build_tx_sender(
        tx_sender_config,
        &api_config.web3_json_rpc,
        state_keeper_config,
        replica_connection_pool.clone(),
        master_connection_pool.clone(),
        gas_adjuster,
        factory_deps_cache.clone(),
    )
    .await;

    let mut builder =
        web3::ApiBuilder::jsonrpsee_backend(internal_api.clone(), replica_connection_pool)
            .http(api_config.web3_json_rpc.http_port)
            .with_filter_limit(api_config.web3_json_rpc.filters_limit())
            .with_threads(api_config.web3_json_rpc.http_server_threads())
            .with_tx_sender(tx_sender);

    if with_debug_namespace {
        builder = builder.enable_debug_namespace(
            BaseSystemContractsHashes {
                bootloader: tx_sender_config.bootloader,
                default_aa: tx_sender_config.default_aa,
            },
            tx_sender_config.fair_l2_gas_price,
            api_config.web3_json_rpc.vm_execution_cache_misses_limit,
        )
    }

    builder.build(stop_receiver.clone()).await
}

#[allow(clippy::too_many_arguments)]
async fn run_ws_api<G: L1GasPriceProvider + Send + Sync + 'static>(
    tx_sender_config: &TxSenderConfig,
    state_keeper_config: &StateKeeperConfig,
    internal_api: &InternalApiConfig,
    api_config: &ApiConfig,
    gas_adjuster: Arc<G>,
    master_connection_pool: ConnectionPool,
    replica_connection_pool: ConnectionPool,
    stop_receiver: watch::Receiver<bool>,
    factory_deps_cache: FactoryDepsCache,
) -> (Vec<JoinHandle<()>>, ApiHealthCheck) {
    let tx_sender = build_tx_sender(
        tx_sender_config,
        &api_config.web3_json_rpc,
        state_keeper_config,
        replica_connection_pool.clone(),
        master_connection_pool.clone(),
        gas_adjuster,
        factory_deps_cache.clone(),
    )
    .await;

    web3::ApiBuilder::jsonrpc_backend(internal_api.clone(), replica_connection_pool)
        .ws(api_config.web3_json_rpc.ws_port)
        .with_filter_limit(api_config.web3_json_rpc.filters_limit())
        .with_subscriptions_limit(api_config.web3_json_rpc.subscriptions_limit())
        .with_polling_interval(api_config.web3_json_rpc.pubsub_interval())
        .with_threads(api_config.web3_json_rpc.ws_server_threads())
        .with_tx_sender(tx_sender)
        .build(stop_receiver.clone())
        .await
}

async fn circuit_breakers_for_components(
    components: &[Component],
    web3_url: &str,
    circuit_breaker_config: &CircuitBreakerConfig,
    main_contract: H160,
) -> Vec<Box<dyn CircuitBreaker>> {
    let mut circuit_breakers: Vec<Box<dyn CircuitBreaker>> = Vec::new();

    if components.iter().any(|c| {
        matches!(
            c,
            Component::EthTxAggregator | Component::EthTxManager | Component::StateKeeper
        )
    }) {
        circuit_breakers.push(Box::new(FailedL1TransactionChecker {
            pool: ConnectionPool::new(Some(1), DbVariant::Replica).await,
        }));
    }

    if components.iter().any(|c| {
        matches!(
            c,
            Component::EthTxAggregator | Component::EthTxManager | Component::TreeBackup
        )
    }) {
        let eth_client = QueryClient::new(web3_url).unwrap();
        circuit_breakers.push(Box::new(VksChecker::new(
            circuit_breaker_config,
            eth_client,
            main_contract,
        )));
    }

    if components
        .iter()
        .any(|c| matches!(c, Component::EthTxAggregator | Component::EthTxManager))
    {
        let eth_client = QueryClient::new(web3_url).unwrap();
        circuit_breakers.push(Box::new(FacetSelectorsChecker::new(
            circuit_breaker_config,
            eth_client,
            main_contract,
        )));
    }

    circuit_breakers
}

#[tokio::test]
async fn test_house_keeper_components_get_added() {
    let (core_task_handles, _, _, _) = initialize_components(vec![Component::Housekeeper], false)
        .await
        .unwrap();
    // circuit-breaker, prometheus-exporter components are run, irrespective of other components.
    let always_running_component_count = 2;
    assert_eq!(13, core_task_handles.len() - always_running_component_count);
}
