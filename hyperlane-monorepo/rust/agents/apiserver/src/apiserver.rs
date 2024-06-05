use std::{
    borrow::{Borrow, BorrowMut}, collections::{HashMap, HashSet}, fmt::{Debug, Formatter}, sync::Arc
};

use async_trait::async_trait;
use derive_more::AsRef;
use eyre::Result;
use hyperlane_base::{
    db::{HyperlaneRocksDB, DB},
    metrics::{AgentMetrics, AgentMetricsUpdater},
    run_all,
    settings::ChainConf,
    BaseAgent, ContractSyncMetrics, CoreMetrics, HyperlaneAgentCore, SequencedDataContractSync,
    WatermarkContractSync,
};
use hyperlane_core::{metrics::agent::METRICS_SCRAPE_INTERVAL, HyperlaneDomain, HyperlaneMessage, InterchainGasPayment, MerkleTreeInsertion, U256};
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        RwLock,
    },
    task::JoinHandle,
};
use tracing::{info, info_span, instrument::Instrumented, Instrument};

use tide::prelude::*;
use tide_tracing::TraceMiddleware;
use hyperlane_ethereum::{SingletonSigner, SingletonSignerHandle};



use crate::merkle_tree::processor::{MerkleTreeProcessor, MerkleTreeProcessorMetrics};
use crate::processor::{Processor, ProcessorExt};
use crate::{api, merkle_tree::builder::MerkleTreeBuilder, msg::{
    gas_payment::GasPaymentEnforcer,
    metadata::{AppContextClassifier, BaseMetadataBuilder},
    pending_message::{MessageContext, MessageSubmissionMetrics},
    pending_operation::DynPendingOperation,
    processor::{MessageProcessor, MessageProcessorMetrics},
    serial_submitter::{SerialSubmitter, SerialSubmitterMetrics},
}, settings::{matching_list::MatchingList, APIServerSettings}};



#[derive(Clone, Debug)]
pub struct State {
    pub origin_chains: HashMap<HyperlaneDomain, ChainConf>,
    pub destination_chains: HashMap<HyperlaneDomain, ChainConf>,
    pub prover_syncs: HashMap<HyperlaneDomain, Arc<RwLock<MerkleTreeBuilder>>>,
    pub dbs: HashMap<HyperlaneDomain, HyperlaneRocksDB>,
    pub whitelist: Arc<MatchingList>,
    pub blacklist: Arc<MatchingList>,
    pub msg_ctxs: HashMap<ContextKey, Arc<MessageContext>>,
    pub signer: SingletonSignerHandle,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ValidityRequest {
    pub domain: HyperlaneDomain,
    pub message: HyperlaneMessage,
    //insertion: MerkleTreeInsertion,
}

impl Default for ValidityRequest {
    fn default() -> Self {
        ValidityRequest {
            domain: HyperlaneDomain::new_test_domain("geth0"),
            message: HyperlaneMessage::default()
        } 
    }
}


#[derive(Debug, Deserialize, Serialize)]
pub struct ValidityResponse {
    pub message: HyperlaneMessage,
    pub metadata: Vec<u8>,
    pub gas_limit: U256,
}

#[derive(Debug, Hash, PartialEq, Eq, Copy, Clone)]
pub struct ContextKey {
    pub origin: u32,
    pub destination: u32,
}

/// An APIServer agent
#[derive(AsRef)]
pub struct APIServer {
    //origin_chains: HashSet<HyperlaneDomain>,
    origin_chains: HashMap<HyperlaneDomain, ChainConf>,
    destination_chains: HashMap<HyperlaneDomain, ChainConf>,
    #[as_ref]
    core: HyperlaneAgentCore,
    message_syncs: HashMap<HyperlaneDomain, Arc<SequencedDataContractSync<HyperlaneMessage>>>,
    interchain_gas_payment_syncs:
        HashMap<HyperlaneDomain, Arc<WatermarkContractSync<InterchainGasPayment>>>,
    /// Context data for each (origin, destination) chain pair a message can be
    /// sent between
    msg_ctxs: HashMap<ContextKey, Arc<MessageContext>>,
    prover_syncs: HashMap<HyperlaneDomain, Arc<RwLock<MerkleTreeBuilder>>>,
    merkle_tree_hook_syncs:
        HashMap<HyperlaneDomain, Arc<SequencedDataContractSync<MerkleTreeInsertion>>>,
    dbs: HashMap<HyperlaneDomain, HyperlaneRocksDB>,
    whitelist: Arc<MatchingList>,
    blacklist: Arc<MatchingList>,
    transaction_gas_limit: Option<U256>,
    skip_transaction_gas_limit_for: HashSet<u32>,
    allow_local_checkpoint_syncers: bool,
    core_metrics: Arc<CoreMetrics>,
    agent_metrics: AgentMetrics,
    signer: SingletonSignerHandle,
    // temporary holder until `run` is called
    signer_instance: Option<Box<SingletonSigner>>,
}

impl Debug for APIServer {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "APIServer {{ origin_chains: {:?}, destination_chains: {:?}, whitelist: {:?}, blacklist: {:?}, transaction_gas_limit: {:?}, skip_transaction_gas_limit_for: {:?}, allow_local_checkpoint_syncers: {:?} }}",
            self.origin_chains,
            self.destination_chains,
            self.whitelist,
            self.blacklist,
            self.transaction_gas_limit,
            self.skip_transaction_gas_limit_for,
            self.allow_local_checkpoint_syncers
        )
    }
}

#[async_trait]
#[allow(clippy::unit_arg)]
impl BaseAgent for APIServer {
    const AGENT_NAME: &'static str = "apiserver";

    type Settings = APIServerSettings;

    async fn from_settings(
        settings: Self::Settings,
        core_metrics: Arc<CoreMetrics>,
        agent_metrics: AgentMetrics,
    ) -> Result<Self>
    where
        Self: Sized,
    {
        let core = settings.build_hyperlane_core(core_metrics.clone());
        let db = DB::from_path(&settings.db)?;
        let dbs = settings
            .origin_chains
            .iter()
            .map(|origin| (origin.clone(), HyperlaneRocksDB::new(origin, db.clone())))
            .collect::<HashMap<_, _>>();


        // Intentionally using hyperlane_ethereum for the validator's signer
        let (signer_instance, signer) = SingletonSigner::new(settings.validator.build().await?);

        let mailboxes = settings
            .build_mailboxes(settings.destination_chains.iter(), &core_metrics)
            .await?;
        let validator_announces = settings
            .build_validator_announces(settings.origin_chains.iter(), &core_metrics)
            .await?;

        let contract_sync_metrics = Arc::new(ContractSyncMetrics::new(&core_metrics));

        let message_syncs = settings
            .build_message_indexers(
                settings.origin_chains.iter(),
                &core_metrics,
                &contract_sync_metrics,
                dbs.iter()
                    .map(|(d, db)| (d.clone(), Arc::new(db.clone()) as _))
                    .collect(),
            )
            .await?;
        let interchain_gas_payment_syncs = settings
            .build_interchain_gas_payment_indexers(
                settings.origin_chains.iter(),
                &core_metrics,
                &contract_sync_metrics,
                dbs.iter()
                    .map(|(d, db)| (d.clone(), Arc::new(db.clone()) as _))
                    .collect(),
            )
            .await?;
        let merkle_tree_hook_syncs = settings
            .build_merkle_tree_hook_indexers(
                settings.origin_chains.iter(),
                &core_metrics,
                &contract_sync_metrics,
                dbs.iter()
                    .map(|(d, db)| (d.clone(), Arc::new(db.clone()) as _))
                    .collect(),
            )
            .await?;

        let whitelist = Arc::new(settings.whitelist);
        let blacklist = Arc::new(settings.blacklist);
        let skip_transaction_gas_limit_for = settings.skip_transaction_gas_limit_for;
        let transaction_gas_limit = settings.transaction_gas_limit;

        info!(
            %whitelist,
            %blacklist,
            ?transaction_gas_limit,
            ?skip_transaction_gas_limit_for,
            "Whitelist configuration"
        );

        // provers by origin chain
        let prover_syncs = settings
            .origin_chains
            .iter()
            .map(|origin| {
                info!("setting up prover {}", origin.id());
                (
                    origin.clone(),
                    Arc::new(RwLock::new(MerkleTreeBuilder::new())),
                )
            })
            .collect::<HashMap<_, _>>();

        info!(gas_enforcement_policies=?settings.gas_payment_enforcement, "Gas enforcement configuration");

        // need one of these per origin chain due to the database scoping even though
        // the config itself is the same
        let gas_payment_enforcers: HashMap<_, _> = settings
            .origin_chains
            .iter()
            .map(|domain| {
                (
                    domain.clone(),
                    Arc::new(GasPaymentEnforcer::new(
                        settings.gas_payment_enforcement.clone(),
                        dbs.get(domain).unwrap().clone(),
                    )),
                )
            })
            .collect();

        let mut msg_ctxs = HashMap::new();
        let mut destination_chains = HashMap::new();

        for destination in &settings.destination_chains {
            let destination_chain_setup = core.settings.chain_setup(destination).unwrap().clone();
            destination_chains.insert(destination.clone(), destination_chain_setup.clone());
            let transaction_gas_limit: Option<U256> =
                if skip_transaction_gas_limit_for.contains(&destination.id()) {
                    None
                } else {
                    transaction_gas_limit
                };

            for origin in &settings.origin_chains {
                let db = dbs.get(origin).unwrap().clone();
                let metadata_builder = BaseMetadataBuilder::new(
                    origin.clone(),
                    destination_chain_setup.clone(),
                    prover_syncs[origin].clone(),
                    validator_announces[origin].clone(),
                    settings.allow_local_checkpoint_syncers,
                    core.metrics.clone(),
                    db,
                    5,
                    AppContextClassifier::new(
                        mailboxes[destination].clone(),
                        settings.metric_app_contexts.clone(),
                    ),
                );

                msg_ctxs.insert(
                    ContextKey {
                        origin: origin.id(),
                        destination: destination.id(),
                    },
                    Arc::new(MessageContext {
                        destination_mailbox: mailboxes[destination].clone(),
                        origin_db: dbs.get(origin).unwrap().clone(),
                        metadata_builder: Arc::new(metadata_builder),
                        origin_gas_payment_enforcer: gas_payment_enforcers[origin].clone(),
                        transaction_gas_limit,
                        metrics: MessageSubmissionMetrics::new(&core_metrics, origin, destination),
                    }),
                );
            }
        }
        let mut origin_chains = HashMap::new();

        for origin in &settings.origin_chains {
            let origin_chain_setup = core.settings.chain_setup(origin).unwrap().clone();
            origin_chains.insert(origin.clone(), origin_chain_setup.clone());
        }


        Ok(Self {
            dbs,
            origin_chains,
            //: settings.origin_chains,
            destination_chains,
            msg_ctxs,
            core,
            message_syncs,
            interchain_gas_payment_syncs,
            prover_syncs,
            merkle_tree_hook_syncs,
            whitelist,
            blacklist,
            transaction_gas_limit,
            skip_transaction_gas_limit_for,
            allow_local_checkpoint_syncers: settings.allow_local_checkpoint_syncers,
            core_metrics,
            agent_metrics,
            signer,
            signer_instance: Some(Box::new(signer_instance)),
        })
    }

    #[allow(clippy::async_yields_async)]
    async fn run(mut self) -> Instrumented<JoinHandle<Result<()>>> {
        let mut tasks = vec![];


        if let Some(signer_instance) = self.signer_instance.take() {
            tasks.push(
                tokio::spawn(async move {
                    signer_instance.run().await;
                    Ok(())
                })
                    .instrument(info_span!("SingletonSigner")),
            );
        }

        // TODO make sure the validator is announced somehow
        // announce the validator after spawning the signer task
        // self.announce().await.expect("Failed to announce validator");

        // send channels by destination chain
        let mut send_channels = HashMap::with_capacity(self.destination_chains.len());
        for (dest_domain, dest_conf) in &self.destination_chains {
            let (send_channel, receive_channel) =
                mpsc::unbounded_channel::<Box<DynPendingOperation>>();
            send_channels.insert(dest_domain.id(), send_channel);

            tasks.push(self.run_destination_submitter(dest_domain, receive_channel));

            let agent_metrics_conf = dest_conf
                .agent_metrics_conf(Self::AGENT_NAME.to_string())
                .await
                .unwrap();
            let agent_metrics_fetcher = dest_conf.build_provider(&self.core_metrics).await.unwrap();
            let agent_metrics = AgentMetricsUpdater::new(
                self.agent_metrics.clone(),
                agent_metrics_conf,
                agent_metrics_fetcher,
            );

            let fetcher_task = tokio::spawn(async move {
                agent_metrics
                    .start_updating_on_interval(METRICS_SCRAPE_INTERVAL)
                    .await;
                Ok(())
            })
            .instrument(info_span!("AgentMetrics"));
            tasks.push(fetcher_task);
        }

        //TODO Step 1 this is where the messages get synced to the database
        for (origin_domain, origin_conf) in &self.origin_chains {
            tasks.push(self.run_message_sync(origin_domain).await);
            tasks.push(self.run_interchain_gas_payment_sync(origin_domain).await);
            tasks.push(self.run_merkle_tree_hook_syncs(origin_domain).await);
        }

        //TODO Step 2 this is where the messages get processed from the initial sync
        // each message process attempts to send messages from a chain
        for (origin_domain, origin_conf) in &self.origin_chains {
            tasks.push(self.run_message_processor(origin_domain, send_channels.clone()));
            tasks.push(self.run_merkle_tree_processor(origin_domain));
        }

        tasks.push(self.run_tide_server().await);

        run_all(tasks)
    }
}

impl APIServer {
    async fn run_message_sync(
        &self,
        origin: &HyperlaneDomain,
    ) -> Instrumented<JoinHandle<eyre::Result<()>>> {
        let index_settings = self.as_ref().settings.chains[origin.name()].index_settings();
        let contract_sync = self.message_syncs.get(origin).unwrap().clone();
        let cursor = contract_sync
            .forward_backward_message_sync_cursor(index_settings)
            .await;
        tokio::spawn(async move {
            contract_sync
                .clone()
                .sync("dispatched_messages", cursor)
                .await
        })
        .instrument(info_span!("ContractSync"))
    }

    async fn run_interchain_gas_payment_sync(
        &self,
        origin: &HyperlaneDomain,
    ) -> Instrumented<JoinHandle<eyre::Result<()>>> {
        let index_settings = self.as_ref().settings.chains[origin.name()].index_settings();
        let contract_sync = self
            .interchain_gas_payment_syncs
            .get(origin)
            .unwrap()
            .clone();
        let cursor = contract_sync.rate_limited_cursor(index_settings).await;
        tokio::spawn(async move { contract_sync.clone().sync("gas_payments", cursor).await })
            .instrument(info_span!("ContractSync"))
    }

    async fn run_merkle_tree_hook_syncs(
        &self,
        origin: &HyperlaneDomain,
    ) -> Instrumented<JoinHandle<eyre::Result<()>>> {
        let index_settings = self.as_ref().settings.chains[origin.name()].index.clone();
        let contract_sync = self.merkle_tree_hook_syncs.get(origin).unwrap().clone();
        let cursor = contract_sync
            .forward_backward_message_sync_cursor(index_settings)
            .await;
        tokio::spawn(async move { contract_sync.clone().sync("merkle_tree_hook", cursor).await })
            .instrument(info_span!("ContractSync"))
    }

    fn run_message_processor(
        &self,
        origin: &HyperlaneDomain,
        send_channels: HashMap<u32, UnboundedSender<Box<DynPendingOperation>>>,
    ) -> Instrumented<JoinHandle<Result<()>>> {
        let metrics = MessageProcessorMetrics::new(
            &self.core.metrics,
            origin,
            self.destination_chains.keys(),
        );
        let destination_ctxs = self
            .destination_chains
            .keys()
            .filter(|&destination| destination != origin)
            .map(|destination| {
                (
                    destination.id(),
                    self.msg_ctxs[&ContextKey {
                        origin: origin.id(),
                        destination: destination.id(),
                    }]
                        .clone(),
                )
            })
            .collect();
        let message_processor = MessageProcessor::new(
            self.dbs.get(origin).unwrap().clone(),
            self.whitelist.clone(),
            self.blacklist.clone(),
            metrics,
            send_channels,
            destination_ctxs,
        );

        let span = info_span!("MessageProcessor", origin=%message_processor.domain());
        let processor = Processor::new(Box::new(message_processor));
        tokio::spawn(async move {
            let res = tokio::try_join!(processor.spawn())?;
            info!(?res, "try_join finished for message processor");
            Ok(())
        })
        .instrument(span)
    }

    fn run_merkle_tree_processor(
        &self,
        origin: &HyperlaneDomain,
    ) -> Instrumented<JoinHandle<Result<()>>> {
        let metrics = MerkleTreeProcessorMetrics::new();
        let merkle_tree_processor = MerkleTreeProcessor::new(
            self.dbs.get(origin).unwrap().clone(),
            metrics,
            self.prover_syncs[origin].clone(),
        );

        let span = info_span!("MerkleTreeProcessor", origin=%merkle_tree_processor.domain());
        let processor = Processor::new(Box::new(merkle_tree_processor));
        tokio::spawn(async move {
            let res = tokio::try_join!(processor.spawn())?;
            info!(?res, "try_join finished for merkle tree processor");
            Ok(())
        })
        .instrument(span)
    }

    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(skip(self, receiver))]
    fn run_destination_submitter(
        &self,
        destination: &HyperlaneDomain,
        receiver: UnboundedReceiver<Box<DynPendingOperation>>,
    ) -> Instrumented<JoinHandle<Result<()>>> {
        let serial_submitter = SerialSubmitter::new(
            destination.clone(),
            receiver,
            SerialSubmitterMetrics::new(&self.core.metrics, destination),
        );
        let span = info_span!("SerialSubmitter", destination=%destination);
        let submit_fut = serial_submitter.spawn();

        tokio::spawn(async move {
            let res = tokio::try_join!(submit_fut)?;
            info!(?res, "try_join finished for submitter");
            Ok(())
        })
        .instrument(span)
    }

    async fn run_tide_server(
        &self,
        //address: std::net::SocketAddr,
        //receiver: UnboundedReceiver<Box<DynPendingOperation>>,
    ) -> Instrumented<JoinHandle<Result<()>>> {
        let state = State {
            origin_chains: self.origin_chains.clone(),
            destination_chains: self.destination_chains.clone(),
            prover_syncs: self.prover_syncs.clone(),
            dbs: self.dbs.clone(),
            whitelist: self.whitelist.clone(),
            blacklist: self.blacklist.clone(),
            msg_ctxs: self.msg_ctxs.clone(),
            signer: self.signer.clone(),
        };

        let mut app = tide::with_state(state);
        app.with(TraceMiddleware::new());

        app.at("/check/validity").post(|req| async {
            let validity_span = info_span!("check validity");
            info!("reach check validity");
            api::validity::check_validity(req).instrument(validity_span).await
        });
        // app.listen("127.0.0.1:8080").await?;
        // Define your Tide routes and handlers here
        app.at("/").get(|_| async { Ok("Hello, Tide!") });

        let server_span = info_span!("TideServer", address = "0.0.0.0:8080");
        let server = app.listen("0.0.0.0:8080");

        info!("api server is on at 0.0.0.0:8080");

        tokio::spawn(async move {
            let res = server.await;
            info!(?res, "Tide server finished");
            Ok(())
        })
        .instrument(server_span)
    }






}

#[cfg(test)]
mod test {
    use super::ValidityRequest;

    #[test]
    fn test_param_serialization() {
        let param = ValidityRequest::default();
        let param_str = serde_json::to_string(&param).unwrap();
        println!("{}", param_str);
    }
}
