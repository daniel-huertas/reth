use crate::{
    engine::hooks::PruneHook, hooks::EngineHooks, BeaconConsensus, BeaconConsensusEngine,
    BeaconConsensusEngineError, BeaconConsensusEngineHandle, BeaconForkChoiceUpdateError,
    BeaconOnNewPayloadError, MIN_BLOCKS_FOR_PIPELINE_RUN,
};
use reth_blockchain_tree::{
    config::BlockchainTreeConfig, externals::TreeExternals, BlockchainTree, ShareableBlockchainTree,
};
use reth_db::{
    test_utils::{create_test_rw_db, TempDatabase},
    DatabaseEnv as DE,
};
type DatabaseEnv = TempDatabase<DE>;
use reth_downloaders::{
    bodies::bodies::BodiesDownloaderBuilder,
    headers::reverse_headers::ReverseHeadersDownloaderBuilder,
};
use reth_interfaces::{
    consensus::Consensus,
    executor::BlockExecutionError,
    p2p::{bodies::client::BodiesClient, either::EitherDownloader, headers::client::HeadersClient},
    sync::NoopSyncStateUpdater,
    test_utils::{NoopFullBlockClient, TestConsensus},
};
use reth_payload_builder::test_utils::spawn_test_payload_service;
use reth_primitives::{BlockNumber, ChainSpec, PruneModes, Receipt, B256, U256};
use reth_provider::{
    providers::BlockchainProvider, test_utils::TestExecutorFactory, BlockExecutor,
    BundleStateWithReceipts, ExecutorFactory, HeaderSyncMode, ProviderFactory,
    PrunableBlockExecutor,
};
use reth_prune::Pruner;
use reth_revm::EvmProcessorFactory;
use reth_rpc_types::engine::{
    CancunPayloadFields, ExecutionPayload, ForkchoiceState, ForkchoiceUpdated, PayloadStatus,
};
use reth_stages::{sets::DefaultStages, test_utils::TestStages, ExecOutput, Pipeline, StageError};
use reth_tasks::TokioTaskExecutor;
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::{oneshot, watch};

type TestBeaconConsensusEngine<Client> = BeaconConsensusEngine<
    Arc<DatabaseEnv>,
    BlockchainProvider<
        Arc<DatabaseEnv>,
        ShareableBlockchainTree<
            Arc<DatabaseEnv>,
            EitherExecutorFactory<TestExecutorFactory, EvmProcessorFactory>,
        >,
    >,
    Arc<EitherDownloader<Client, NoopFullBlockClient>>,
>;

#[derive(Debug)]
pub struct TestEnv<DB> {
    pub db: DB,
    // Keep the tip receiver around, so it's not dropped.
    #[allow(dead_code)]
    tip_rx: watch::Receiver<B256>,
    engine_handle: BeaconConsensusEngineHandle,
}

impl<DB> TestEnv<DB> {
    fn new(
        db: DB,
        tip_rx: watch::Receiver<B256>,
        engine_handle: BeaconConsensusEngineHandle,
    ) -> Self {
        Self { db, tip_rx, engine_handle }
    }

    pub async fn send_new_payload<T: Into<ExecutionPayload>>(
        &self,
        payload: T,
        cancun_fields: Option<CancunPayloadFields>,
    ) -> Result<PayloadStatus, BeaconOnNewPayloadError> {
        self.engine_handle.new_payload(payload.into(), cancun_fields).await
    }

    /// Sends the `ExecutionPayload` message to the consensus engine and retries if the engine
    /// is syncing.
    pub async fn send_new_payload_retry_on_syncing<T: Into<ExecutionPayload>>(
        &self,
        payload: T,
        cancun_fields: Option<CancunPayloadFields>,
    ) -> Result<PayloadStatus, BeaconOnNewPayloadError> {
        let payload: ExecutionPayload = payload.into();
        loop {
            let result = self.send_new_payload(payload.clone(), cancun_fields.clone()).await?;
            if !result.is_syncing() {
                return Ok(result)
            }
        }
    }

    pub async fn send_forkchoice_updated(
        &self,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, BeaconForkChoiceUpdateError> {
        self.engine_handle.fork_choice_updated(state, None).await
    }

    /// Sends the `ForkchoiceUpdated` message to the consensus engine and retries if the engine
    /// is syncing.
    pub async fn send_forkchoice_retry_on_syncing(
        &self,
        state: ForkchoiceState,
    ) -> Result<ForkchoiceUpdated, BeaconForkChoiceUpdateError> {
        loop {
            let result = self.engine_handle.fork_choice_updated(state, None).await?;
            if !result.is_syncing() {
                return Ok(result)
            }
        }
    }
}

// TODO: add with_consensus in case we want to use the TestConsensus purposeful failure - this
// would require similar patterns to how we use with_client and the EitherDownloader
/// Represents either a real consensus engine, or a test consensus engine.
#[derive(Debug, Default)]
enum TestConsensusConfig {
    /// Test consensus engine
    #[default]
    Test,
    /// Real consensus engine
    Real,
}

/// Represents either test pipeline outputs, or real pipeline configuration.
#[derive(Debug)]
enum TestPipelineConfig {
    /// Test pipeline outputs.
    Test(VecDeque<Result<ExecOutput, StageError>>),
    /// Real pipeline configuration.
    Real,
}

impl Default for TestPipelineConfig {
    fn default() -> Self {
        Self::Test(VecDeque::new())
    }
}

/// Represents either test executor results, or real executor configuration.
#[derive(Debug)]
enum TestExecutorConfig {
    /// Test executor results.
    Test(Vec<BundleStateWithReceipts>),
    /// Real executor configuration.
    Real,
}

impl Default for TestExecutorConfig {
    fn default() -> Self {
        Self::Test(Vec::new())
    }
}

/// A type that represents one of two possible executor factories.
#[derive(Debug, Clone)]
pub enum EitherExecutorFactory<A: ExecutorFactory, B: ExecutorFactory> {
    /// The first factory variant
    Left(A),
    /// The second factory variant
    Right(B),
}

// A type that represents one of two possible BlockExecutor types.
#[derive(Debug)]
pub enum EitherBlockExecutor<A, B> {
    /// The first executor variant
    Left(A),
    /// The second executor variant
    Right(B),
}

impl<A, B> BlockExecutor for EitherBlockExecutor<A, B>
where
    A: BlockExecutor,
    B: BlockExecutor,
{
    fn execute(
        &mut self,
        block: &reth_primitives::Block,
        total_difficulty: U256,
        senders: Option<Vec<reth_primitives::Address>>,
    ) -> Result<(), BlockExecutionError> {
        match self {
            EitherBlockExecutor::Left(a) => a.execute(block, total_difficulty, senders),
            EitherBlockExecutor::Right(b) => b.execute(block, total_difficulty, senders),
        }
    }

    fn execute_and_verify_receipt(
        &mut self,
        block: &reth_primitives::Block,
        total_difficulty: U256,
        senders: Option<Vec<reth_primitives::Address>>,
    ) -> Result<(), BlockExecutionError> {
        match self {
            EitherBlockExecutor::Left(a) => {
                a.execute_and_verify_receipt(block, total_difficulty, senders)
            }
            EitherBlockExecutor::Right(b) => {
                b.execute_and_verify_receipt(block, total_difficulty, senders)
            }
        }
    }

    fn execute_transactions(
        &mut self,
        block: &reth_primitives::Block,
        total_difficulty: U256,
        senders: Option<Vec<reth_primitives::Address>>,
    ) -> Result<(Vec<Receipt>, u64), BlockExecutionError> {
        match self {
            EitherBlockExecutor::Left(a) => {
                a.execute_transactions(block, total_difficulty, senders)
            }
            EitherBlockExecutor::Right(b) => {
                b.execute_transactions(block, total_difficulty, senders)
            }
        }
    }

    fn take_output_state(&mut self) -> BundleStateWithReceipts {
        match self {
            EitherBlockExecutor::Left(a) => a.take_output_state(),
            EitherBlockExecutor::Right(b) => b.take_output_state(),
        }
    }

    fn stats(&self) -> reth_provider::BlockExecutorStats {
        match self {
            EitherBlockExecutor::Left(a) => a.stats(),
            EitherBlockExecutor::Right(b) => b.stats(),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        match self {
            EitherBlockExecutor::Left(a) => a.size_hint(),
            EitherBlockExecutor::Right(b) => b.size_hint(),
        }
    }
}

impl<A, B> PrunableBlockExecutor for EitherBlockExecutor<A, B>
where
    B: PrunableBlockExecutor,
    A: PrunableBlockExecutor,
{
    fn set_prune_modes(&mut self, prune_modes: PruneModes) {
        match self {
            EitherBlockExecutor::Left(a) => a.set_prune_modes(prune_modes),
            EitherBlockExecutor::Right(b) => b.set_prune_modes(prune_modes),
        }
    }

    fn set_tip(&mut self, tip: BlockNumber) {
        match self {
            EitherBlockExecutor::Left(a) => a.set_tip(tip),
            EitherBlockExecutor::Right(b) => b.set_tip(tip),
        }
    }
}

impl<A, B> ExecutorFactory for EitherExecutorFactory<A, B>
where
    A: ExecutorFactory,
    B: ExecutorFactory,
{
    fn chain_spec(&self) -> &ChainSpec {
        match self {
            EitherExecutorFactory::Left(a) => a.chain_spec(),
            EitherExecutorFactory::Right(b) => b.chain_spec(),
        }
    }

    fn with_state<'a, SP: reth_provider::StateProvider + 'a>(
        &'a self,
        sp: SP,
    ) -> Box<dyn PrunableBlockExecutor + 'a> {
        match self {
            EitherExecutorFactory::Left(a) => a.with_state::<'a, SP>(sp),
            EitherExecutorFactory::Right(b) => b.with_state::<'a, SP>(sp),
        }
    }
}

/// The basic configuration for a `TestConsensusEngine`, without generics for the client or
/// consensus engine.
#[derive(Debug)]
pub struct TestConsensusEngineBuilder {
    chain_spec: Arc<ChainSpec>,
    pipeline_config: TestPipelineConfig,
    executor_config: TestExecutorConfig,
    pipeline_run_threshold: Option<u64>,
    max_block: Option<BlockNumber>,
    consensus: TestConsensusConfig,
}

impl TestConsensusEngineBuilder {
    /// Create a new `TestConsensusEngineBuilder` with the given `ChainSpec`.
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            chain_spec,
            pipeline_config: Default::default(),
            executor_config: Default::default(),
            pipeline_run_threshold: None,
            max_block: None,
            consensus: Default::default(),
        }
    }

    /// Set the pipeline execution outputs to use for the test consensus engine.
    pub fn with_pipeline_exec_outputs(
        mut self,
        pipeline_exec_outputs: VecDeque<Result<ExecOutput, StageError>>,
    ) -> Self {
        self.pipeline_config = TestPipelineConfig::Test(pipeline_exec_outputs);
        self
    }

    /// Set the executor results to use for the test consensus engine.
    pub fn with_executor_results(mut self, executor_results: Vec<BundleStateWithReceipts>) -> Self {
        self.executor_config = TestExecutorConfig::Test(executor_results);
        self
    }

    /// Sets the max block for the pipeline to run.
    pub fn with_max_block(mut self, max_block: BlockNumber) -> Self {
        self.max_block = Some(max_block);
        self
    }

    /// Uses the real pipeline instead of a pipeline with empty exec outputs.
    pub fn with_real_pipeline(mut self) -> Self {
        self.pipeline_config = TestPipelineConfig::Real;
        self
    }

    /// Uses the real executor instead of a executor with empty results.
    pub fn with_real_executor(mut self) -> Self {
        self.executor_config = TestExecutorConfig::Real;
        self
    }

    /// Uses a real consensus engine instead of a test consensus engine.
    pub fn with_real_consensus(mut self) -> Self {
        self.consensus = TestConsensusConfig::Real;
        self
    }

    /// Disables blockchain tree driven sync. This is the same as setting the pipeline run
    /// threshold to 0.
    pub fn disable_blockchain_tree_sync(mut self) -> Self {
        self.pipeline_run_threshold = Some(0);
        self
    }

    /// Sets the client to use for network operations.
    #[allow(dead_code)]
    pub fn with_client<Client>(self, client: Client) -> NetworkedTestConsensusEngineBuilder<Client>
    where
        Client: HeadersClient + BodiesClient + 'static,
    {
        NetworkedTestConsensusEngineBuilder { base_config: self, client: Some(client) }
    }

    /// Builds the test consensus engine into a `TestConsensusEngine` and `TestEnv`.
    pub fn build(
        self,
    ) -> (TestBeaconConsensusEngine<NoopFullBlockClient>, TestEnv<Arc<DatabaseEnv>>) {
        let networked = NetworkedTestConsensusEngineBuilder { base_config: self, client: None };

        networked.build()
    }
}

/// A builder for `TestConsensusEngine`, allows configuration of mocked pipeline outputs and
/// mocked executor results.
///
/// This optionally includes a client for network operations.
#[derive(Debug)]
pub struct NetworkedTestConsensusEngineBuilder<Client> {
    base_config: TestConsensusEngineBuilder,
    client: Option<Client>,
}

impl<Client> NetworkedTestConsensusEngineBuilder<Client>
where
    Client: HeadersClient + BodiesClient + 'static,
{
    /// Set the pipeline execution outputs to use for the test consensus engine.
    #[allow(dead_code)]
    pub fn with_pipeline_exec_outputs(
        mut self,
        pipeline_exec_outputs: VecDeque<Result<ExecOutput, StageError>>,
    ) -> Self {
        self.base_config.pipeline_config = TestPipelineConfig::Test(pipeline_exec_outputs);
        self
    }

    /// Set the executor results to use for the test consensus engine.
    #[allow(dead_code)]
    pub fn with_executor_results(mut self, executor_results: Vec<BundleStateWithReceipts>) -> Self {
        self.base_config.executor_config = TestExecutorConfig::Test(executor_results);
        self
    }

    /// Sets the max block for the pipeline to run.
    #[allow(dead_code)]
    pub fn with_max_block(mut self, max_block: BlockNumber) -> Self {
        self.base_config.max_block = Some(max_block);
        self
    }

    /// Uses the real pipeline instead of a pipeline with empty exec outputs.
    #[allow(dead_code)]
    pub fn with_real_pipeline(mut self) -> Self {
        self.base_config.pipeline_config = TestPipelineConfig::Real;
        self
    }

    /// Uses the real executor instead of a executor with empty results.
    #[allow(dead_code)]
    pub fn with_real_executor(mut self) -> Self {
        self.base_config.executor_config = TestExecutorConfig::Real;
        self
    }

    /// Disables blockchain tree driven sync. This is the same as setting the pipeline run
    /// threshold to 0.
    #[allow(dead_code)]
    pub fn disable_blockchain_tree_sync(mut self) -> Self {
        self.base_config.pipeline_run_threshold = Some(0);
        self
    }

    /// Sets the client to use for network operations.
    #[allow(dead_code)]
    pub fn with_client<ClientType>(
        self,
        client: ClientType,
    ) -> NetworkedTestConsensusEngineBuilder<ClientType>
    where
        ClientType: HeadersClient + BodiesClient + 'static,
    {
        NetworkedTestConsensusEngineBuilder { base_config: self.base_config, client: Some(client) }
    }

    /// Builds the test consensus engine into a `TestConsensusEngine` and `TestEnv`.
    pub fn build(self) -> (TestBeaconConsensusEngine<Client>, TestEnv<Arc<DatabaseEnv>>) {
        reth_tracing::init_test_tracing();
        let db = create_test_rw_db();
        let provider_factory =
            ProviderFactory::new(db.clone(), self.base_config.chain_spec.clone());

        let consensus: Arc<dyn Consensus> = match self.base_config.consensus {
            TestConsensusConfig::Real => {
                Arc::new(BeaconConsensus::new(Arc::clone(&self.base_config.chain_spec)))
            }
            TestConsensusConfig::Test => Arc::new(TestConsensus::default()),
        };
        let payload_builder = spawn_test_payload_service();

        // use either noop client or a user provided client (for example TestFullBlockClient)
        let client = Arc::new(
            self.client
                .map(EitherDownloader::Left)
                .unwrap_or_else(|| EitherDownloader::Right(NoopFullBlockClient::default())),
        );

        // use either test executor or real executor
        let executor_factory = match self.base_config.executor_config {
            TestExecutorConfig::Test(results) => {
                let executor_factory =
                    TestExecutorFactory::new(self.base_config.chain_spec.clone());
                executor_factory.extend(results);
                EitherExecutorFactory::Left(executor_factory)
            }
            TestExecutorConfig::Real => EitherExecutorFactory::Right(EvmProcessorFactory::new(
                self.base_config.chain_spec.clone(),
            )),
        };

        // Setup pipeline
        let (tip_tx, tip_rx) = watch::channel(B256::default());
        let mut pipeline = match self.base_config.pipeline_config {
            TestPipelineConfig::Test(outputs) => Pipeline::builder()
                .add_stages(TestStages::new(outputs, Default::default()))
                .with_tip_sender(tip_tx),
            TestPipelineConfig::Real => {
                let header_downloader = ReverseHeadersDownloaderBuilder::default()
                    .build(client.clone(), consensus.clone())
                    .into_task();

                let body_downloader = BodiesDownloaderBuilder::default()
                    .build(client.clone(), consensus.clone(), provider_factory.clone())
                    .into_task();

                Pipeline::builder().add_stages(DefaultStages::new(
                    ProviderFactory::new(db.clone(), self.base_config.chain_spec.clone()),
                    HeaderSyncMode::Tip(tip_rx.clone()),
                    Arc::clone(&consensus),
                    header_downloader,
                    body_downloader,
                    executor_factory.clone(),
                ))
            }
        };

        if let Some(max_block) = self.base_config.max_block {
            pipeline = pipeline.with_max_block(max_block);
        }

        let pipeline = pipeline.build(provider_factory.clone());

        // Setup blockchain tree
        let externals = TreeExternals::new(provider_factory.clone(), consensus, executor_factory);
        let config = BlockchainTreeConfig::new(1, 2, 3, 2);
        let tree = ShareableBlockchainTree::new(
            BlockchainTree::new(externals, config, None).expect("failed to create tree"),
        );
        let latest = self.base_config.chain_spec.genesis_header().seal_slow();
        let blockchain_provider = BlockchainProvider::with_latest(provider_factory, tree, latest);

        let pruner = Pruner::new(
            db.clone(),
            self.base_config.chain_spec.clone(),
            vec![],
            5,
            self.base_config.chain_spec.prune_delete_limit,
            config.max_reorg_depth() as usize,
            watch::channel(None).1,
        );

        let mut hooks = EngineHooks::new();
        hooks.add(PruneHook::new(pruner, Box::<TokioTaskExecutor>::default()));

        let (mut engine, handle) = BeaconConsensusEngine::new(
            client,
            pipeline,
            blockchain_provider,
            Box::<TokioTaskExecutor>::default(),
            Box::<NoopSyncStateUpdater>::default(),
            None,
            false,
            payload_builder,
            None,
            self.base_config.pipeline_run_threshold.unwrap_or(MIN_BLOCKS_FOR_PIPELINE_RUN),
            hooks,
        )
        .expect("failed to create consensus engine");

        if let Some(max_block) = self.base_config.max_block {
            engine.sync.set_max_block(max_block)
        }

        (engine, TestEnv::new(db, tip_rx, handle))
    }
}

pub fn spawn_consensus_engine<Client: HeadersClient + BodiesClient + 'static>(
    engine: TestBeaconConsensusEngine<Client>,
) -> oneshot::Receiver<Result<(), BeaconConsensusEngineError>> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let result = engine.await;
        tx.send(result).expect("failed to forward consensus engine result");
    });
    rx
}
