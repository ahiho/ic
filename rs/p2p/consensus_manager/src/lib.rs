use std::sync::Arc;

use crate::{
    metrics::ConsensusManagerMetrics,
    receiver::{build_axum_router, ConsensusManagerReceiver},
    sender::ConsensusManagerSender,
};
use axum::Router;
use ic_base_types::NodeId;
use ic_interfaces::p2p::consensus::{ArtifactAssembler, ArtifactTransmit};
use ic_logger::ReplicaLogger;
use ic_metrics::MetricsRegistry;
use ic_quic_transport::{ConnId, Shutdown, SubnetTopology, Transport};
use ic_types::artifact::{IdentifiableArtifact, PbArtifact, UnvalidatedArtifactMutation};
use phantom_newtype::AmountOf;
use tokio::{
    runtime::Handle,
    sync::{
        mpsc::{Receiver, UnboundedSender},
        watch,
    },
};

mod metrics;
mod receiver;
mod sender;

type StartConsensusManagerFn =
    Box<dyn FnOnce(Arc<dyn Transport>, watch::Receiver<SubnetTopology>) -> Vec<Shutdown>>;

pub struct ConsensusManagerBuilder {
    log: ReplicaLogger,
    metrics_registry: MetricsRegistry,
    rt_handle: Handle,
    clients: Vec<StartConsensusManagerFn>,
    router: Option<Router>,
}

impl ConsensusManagerBuilder {
    pub fn new(log: ReplicaLogger, rt_handle: Handle, metrics_registry: MetricsRegistry) -> Self {
        Self {
            log,
            metrics_registry,
            rt_handle,
            clients: Vec::new(),
            router: None,
        }
    }

    pub fn add_client<
        Artifact: IdentifiableArtifact,
        WireArtifact: PbArtifact,
        F: FnOnce(Arc<dyn Transport>) -> D + 'static,
        D: ArtifactAssembler<Artifact, WireArtifact>,
    >(
        &mut self,
        outbound_artifacts_rx: Receiver<ArtifactTransmit<Artifact>>,
        inbound_artifacts_tx: UnboundedSender<UnvalidatedArtifactMutation<Artifact>>,
        (assembler, assembler_router): (F, Router),
        slot_limit: usize,
    ) {
        assert!(uri_prefix::<WireArtifact>()
            .chars()
            .all(char::is_alphabetic));
        let (router, adverts_from_peers_rx) = build_axum_router(self.log.clone());

        let log = self.log.clone();
        let rt_handle = self.rt_handle.clone();
        let metrics_registry = self.metrics_registry.clone();

        let builder = move |transport: Arc<dyn Transport>, topology_watcher| {
            start_consensus_manager(
                log,
                &metrics_registry,
                rt_handle,
                outbound_artifacts_rx,
                adverts_from_peers_rx,
                inbound_artifacts_tx,
                assembler(transport.clone()),
                transport,
                topology_watcher,
                slot_limit,
            )
        };

        self.router = Some(
            self.router
                .take()
                .unwrap_or_default()
                .merge(router)
                .merge(assembler_router),
        );

        self.clients.push(Box::new(builder));
    }

    pub fn router(&mut self) -> Router {
        self.router.take().unwrap_or_default()
    }

    pub fn run(
        self,
        transport: Arc<dyn Transport>,
        topology_watcher: watch::Receiver<SubnetTopology>,
    ) -> Vec<Shutdown> {
        let mut ret = vec![];
        for client in self.clients {
            ret.append(&mut client(transport.clone(), topology_watcher.clone()));
        }
        ret
    }
}

fn start_consensus_manager<Artifact, WireArtifact, Assembler>(
    log: ReplicaLogger,
    metrics_registry: &MetricsRegistry,
    rt_handle: Handle,
    // Locally produced adverts to send to the node's peers.
    outbound_transmits: Receiver<ArtifactTransmit<Artifact>>,
    // Adverts received from peers
    adverts_received: Receiver<(SlotUpdate<WireArtifact>, NodeId, ConnId)>,
    sender: UnboundedSender<UnvalidatedArtifactMutation<Artifact>>,
    assembler: Assembler,
    transport: Arc<dyn Transport>,
    topology_watcher: watch::Receiver<SubnetTopology>,
    slot_limit: usize,
) -> Vec<Shutdown>
where
    Artifact: IdentifiableArtifact,
    WireArtifact: PbArtifact,
    Assembler: ArtifactAssembler<Artifact, WireArtifact>,
{
    let metrics = ConsensusManagerMetrics::new::<WireArtifact>(metrics_registry);

    let shutdown_send_side = ConsensusManagerSender::<Artifact, WireArtifact, _>::run(
        log.clone(),
        metrics.clone(),
        rt_handle.clone(),
        transport.clone(),
        outbound_transmits,
        assembler.clone(),
    );

    let shutdown_receive_side = ConsensusManagerReceiver::run(
        log,
        metrics,
        rt_handle,
        adverts_received,
        assembler,
        sender,
        topology_watcher,
        slot_limit,
    );
    vec![shutdown_send_side, shutdown_receive_side]
}

struct SlotUpdate<Artifact: PbArtifact> {
    slot_number: SlotNumber,
    commit_id: CommitId,
    update: Update<Artifact>,
}

enum Update<Artifact: PbArtifact> {
    Artifact(Artifact),
    Id(Artifact::Id),
}

fn uri_prefix<Artifact: PbArtifact>() -> String {
    Artifact::NAME.to_lowercase()
}

struct SlotNumberTag;
type SlotNumber = AmountOf<SlotNumberTag, u64>;

struct CommitIdTag;
type CommitId = AmountOf<CommitIdTag, u64>;
