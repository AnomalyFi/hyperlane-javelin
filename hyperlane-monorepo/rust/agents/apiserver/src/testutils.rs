use std::collections::HashMap;
use std::fmt::Debug;
use std::path::Path;
use std::sync::Arc;
use async_trait::async_trait;
use base::DefaultIsmCache;
use ethers::prelude::Signature;
use ethers_prometheus::middleware::{PrometheusMiddlewareConf};
use mpsc::unbounded_channel;
use prometheus::{IntCounter, IntGauge, Registry};
use tokio::sync::{mpsc, oneshot, RwLock};
use hyperlane_base::CoreMetrics;
use hyperlane_base::db::{DB, HyperlaneRocksDB};
use hyperlane_base::settings::{ChainConf, ChainConnectionConf, CoreContractAddresses, IndexSettings};
use hyperlane_core::{H256, HyperlaneChain, HyperlaneDomain, HyperlaneProvider, HyperlaneSigner, HyperlaneSignerError};
use hyperlane_core::error::ChainResult;
use hyperlane_core::types::*;
use hyperlane_ethereum::{Signers, SingletonSignerHandle};
use hyperlane_test::mocks::*;
use mockall::mock;
use tempdir::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;
use crate::apiserver::{ContextKey, State};
use crate::merkle_tree::builder::MerkleTreeBuilder;
use crate::msg;
use crate::msg::gas_payment::{GasPaymentEnforcer, GasPaymentPolicy};
use crate::msg::metadata::{AppContextClassifier, base, BaseMetadataBuilder};
use crate::msg::pending_message::{MessageContext, MessageSubmissionMetrics};
use crate::settings::matching_list::MatchingList;

type Callback = oneshot::Sender<Result<Signature, HyperlaneSignerError>>;
type SignTask = (H256, Callback);

mock! {
    pub HyperlaneProvider {}

    #[async_trait]
    impl HyperlaneProvider for HyperlaneProvider {
        async fn get_block_by_hash(&self, hash: &H256) -> ChainResult<BlockInfo>;
        async fn get_txn_by_hash(&self, hash: &H256) -> ChainResult<TxnInfo>;
        async fn is_contract(&self, address: &H256) -> ChainResult<bool>;
        async fn get_balance(&self, address: String) -> ChainResult<U256>;
    }

    #[async_trait]
    impl HyperlaneChain for HyperlaneProvider {
        fn domain(&self) -> &HyperlaneDomain;
        fn provider(&self) -> Box<dyn HyperlaneProvider>;
    }
}

impl Debug for MockHyperlaneProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MockHyperlaneProvider")
    }
}

/// Helper class for holding test state. Goal is to make writing test scenarios easier.
pub struct TestFixture {
    pub from_domain: HyperlaneDomain,
    pub to_domain: HyperlaneDomain,
    pub state: State,
}

impl TestFixture {
    pub fn new () -> TestFixture {
        let from_domain= HyperlaneDomain::new_test_domain("from_domain", 1);
        let to_domain =  HyperlaneDomain::new_test_domain("to_domain", 2);

        let test_temp_dir = TempDir::new("unit_test").expect("Failed to create temporary unit test directory");

        TestFixture {
            from_domain: from_domain.clone(),
            to_domain: to_domain.clone(),
            state: create_test_state(&from_domain, &to_domain, test_temp_dir.path()),
        }
    }

    pub fn get_from_domain(&self) -> HyperlaneDomain {
        return self.from_domain.clone();
    }

    pub fn get_from_domain_id(&self) -> u32 {
        return self.from_domain.id();
    }

    pub fn get_to_domain_id(&self) -> u32 {
        return self.to_domain.id();
    }

    pub fn get_state(&self) -> &State {
        return &self.state;
    }
}

pub fn create_test_state(from_domain: &HyperlaneDomain,
                         to_domain: &HyperlaneDomain,
                         test_path: &Path) -> State {
    let signer: Signers =
        "1111111111111111111111111111111111111111111111111111111111111111"
            .parse::<ethers::signers::LocalWallet>()
            .unwrap()
            .into();
    //let signerConf : SignerConf = SignerConf::HexKey { key: H256::random() };
    //let signers: Signers;

    let from_prover = Arc::new(RwLock::new(MerkleTreeBuilder::new()));
    let to_prover = Arc::new(RwLock::new(MerkleTreeBuilder::new()));

    let mut prover_syncs: HashMap<HyperlaneDomain, Arc<RwLock<MerkleTreeBuilder>>> = HashMap::new();
    prover_syncs.insert(from_domain.clone(), from_prover.clone());
    prover_syncs.insert(to_domain.clone(), to_prover.clone());

    let db = DB::from_path(test_path).unwrap();
    let from_rocks_db: HyperlaneRocksDB = HyperlaneRocksDB::new(&from_domain, db.clone());
    let to_rocks_db: HyperlaneRocksDB = HyperlaneRocksDB::new(&to_domain, db.clone());

    let mut dbs: HashMap<HyperlaneDomain, HyperlaneRocksDB> = HashMap::new();
    dbs.insert(from_domain.clone(), from_rocks_db.clone());
    dbs.insert(to_domain.clone(), to_rocks_db.clone());

    let from_chain_conf = create_test_chain_conf(from_domain.clone());
    let mut origin_chains: HashMap<HyperlaneDomain, ChainConf> = HashMap::new();
    origin_chains.insert(from_domain.clone(), from_chain_conf);

    let to_chain_conf = create_test_chain_conf(to_domain.clone());
    let mut dest_chains: HashMap<HyperlaneDomain, ChainConf> = HashMap::new();
    dest_chains.insert(to_domain.clone(), to_chain_conf.clone());

    let context_key = ContextKey {
        origin: from_domain.id(),
        destination: to_domain.id(),
    };

    let mut mailbox = MockMailboxContract::new();
    mailbox.expect__delivered().returning(|_| Ok(false));
    mailbox.expect__provider().returning(|| {
        let mut provider = MockHyperlaneProvider::new();
        provider.expect_is_contract().returning(|_| Ok(true));
        Box::new(provider)
    });
    mailbox.expect_process_estimate_costs().returning(
        |_, _| Ok(TxCostEstimate {
            gas_limit: Default::default(),
            gas_price: Default::default(),
            l2_gas_limit: None,
        }));

    let mailbox_wrapper = Arc::new(mailbox);

    let app_context_classifier = AppContextClassifier {
        default_ism: DefaultIsmCache {
            value: RwLock::new(None),
            mailbox: mailbox_wrapper.clone(),
        },
        app_matching_lists: Vec::new(),
    };

    let validator_announce = Arc::new(MockValidatorAnnounceContract::new());
    let core_metrics = Arc::new(CoreMetrics::new("test", 12348, Registry::new()).unwrap());

    let builder = BaseMetadataBuilder {
        origin_domain: from_domain.clone(),
        destination_chain_setup: to_chain_conf.clone(),
        origin_prover_sync: from_prover.clone(),
        origin_validator_announce: validator_announce,
        allow_local_checkpoint_syncers: true,
        metrics: core_metrics.clone(),
        db: from_rocks_db.clone(),
        max_depth: 32,
        app_context_classifier,
    };

    let test_gas_policy: Box<dyn GasPaymentPolicy> = Box::new(msg::gas_payment::GasPaymentPolicyNone);
    let matching_list_policy = MatchingList::default();
    let test_gas_enforcer_policy: (Box<dyn GasPaymentPolicy>, MatchingList) = (test_gas_policy, matching_list_policy);

    let gas_payment_enforcer = Arc::new(GasPaymentEnforcer {
        policies: vec!(test_gas_enforcer_policy),
        db: from_rocks_db.clone(),
    });

    let message_ctx = Arc::new(MessageContext {
        destination_mailbox: mailbox_wrapper.clone(),
        origin_db: from_rocks_db.clone(),
        metadata_builder: Arc::new(builder),
        origin_gas_payment_enforcer: gas_payment_enforcer.clone(),
        transaction_gas_limit: None,
        metrics: MessageSubmissionMetrics {
            last_known_nonce: IntGauge::new("test", "test").unwrap(),
            messages_processed: IntCounter::new("test", "test").unwrap(),
        }
    });

    let mut msg_ctxs: HashMap<ContextKey, Arc<MessageContext>> = HashMap::new();
    msg_ctxs.insert(context_key, message_ctx.clone());

    let (tx, rx) = unbounded_channel::<SignTask>();
    let address = signer.eth_address();

    tokio::spawn(async move {
        process_test_signed_request(rx).await;
    });

    let signer_handle = SingletonSignerHandle { address, tx };
    let state = State {
        origin_chains,
        destination_chains: dest_chains,
        prover_syncs,
        dbs,
        whitelist: Arc::new(MatchingList::default()),
        blacklist: Arc::new(MatchingList::default()),
        msg_ctxs,
        signer: signer_handle,
    };
    state
}

pub fn create_test_chain_conf(domain: HyperlaneDomain) -> ChainConf {
    let eth_connection_conf = hyperlane_ethereum::ConnectionConf::Http{url: "http://localhost".parse().unwrap() };

    let prom_middleware_conf = PrometheusMiddlewareConf{
        contracts: Default::default(),
        chain: None
    };

    let index_settings = IndexSettings {
        from: 0,
        chunk_size: 0,
        mode: Default::default(),
    };

    let chain_conf = ChainConf {
        domain,
        signer: None,
        reorg_period: 0,
        addresses: CoreContractAddresses::default(),
        connection: ChainConnectionConf::Ethereum(eth_connection_conf),
        metrics_conf: prom_middleware_conf,
        index: index_settings,
    };
    chain_conf
}

// Handler function to receive a message from the oneshot channel
async fn process_test_signed_request(mut rx: UnboundedReceiver<SignTask>) {
    while let Some(sign_task) = rx.recv().await {
        let signature = Signature {
            r: U256::zero().into(),
            /// S Value
            s: U256::zero().into(),
            /// V value
            v: 0,
        };

        sign_task.1.send(Ok(signature)).unwrap();
    }
}