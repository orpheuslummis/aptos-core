// Copyright © Aptos Foundation

use super::dag_test;
use crate::{
    dag::{bootstrap::bootstrap_dag_for_test, dag_state_sync::StateSyncStatus},
    experimental::buffer_manager::OrderedBlocks,
    network::{IncomingDAGRequest, NetworkSender},
    network_interface::{ConsensusMsg, ConsensusNetworkClient, DIRECT_SEND, RPC},
    network_tests::{NetworkPlayground, TwinId},
    payload_manager::PayloadManager,
    test_utils::{consensus_runtime, EmptyStateComputer, MockPayloadManager, MockStorage},
};
use aptos_channels::{aptos_channel, message_queues::QueueStyle};
use aptos_config::network_id::{NetworkId, PeerNetworkId};
use aptos_consensus_types::common::Author;
use aptos_logger::debug;
use aptos_network::{
    application::interface::NetworkClient,
    peer_manager::{conn_notifs_channel, ConnectionRequestSender, PeerManagerRequestSender},
    protocols::{
        network::{self, Event, NetworkEvents, NewNetworkEvents, NewNetworkSender},
        wire::handshake::v1::ProtocolIdSet,
    },
    transport::ConnectionMetadata,
    ProtocolId,
};
use aptos_time_service::TimeService;
use aptos_types::{
    epoch_state::EpochState,
    ledger_info::generate_ledger_info_with_sig,
    validator_signer::ValidatorSigner,
    validator_verifier::{random_validator_verifier, ValidatorVerifier},
};
use claims::assert_gt;
use futures::{
    stream::{select, Select},
    StreamExt,
};
use futures_channel::mpsc::UnboundedReceiver;
use maplit::hashmap;
use std::sync::Arc;
use tokio::task::JoinHandle;

struct DagBootstrapUnit {
    nh_task_handle: JoinHandle<StateSyncStatus>,
    df_task_handle: JoinHandle<()>,
    dag_rpc_tx: aptos_channel::Sender<Author, IncomingDAGRequest>,
    network_events:
        Box<Select<NetworkEvents<ConsensusMsg>, aptos_channels::Receiver<Event<ConsensusMsg>>>>,
}

impl DagBootstrapUnit {
    fn make(
        self_peer: Author,
        epoch: u64,
        signer: ValidatorSigner,
        storage: Arc<MockStorage>,
        network: NetworkSender,
        time_service: TimeService,
        network_events: Box<
            Select<NetworkEvents<ConsensusMsg>, aptos_channels::Receiver<Event<ConsensusMsg>>>,
        >,
        all_signers: Vec<ValidatorSigner>,
    ) -> (Self, UnboundedReceiver<OrderedBlocks>) {
        let epoch_state = EpochState {
            epoch,
            verifier: storage.get_validator_set().into(),
        };
        let ledger_info = generate_ledger_info_with_sig(&all_signers, storage.get_ledger_info());
        let dag_storage = dag_test::MockStorage::new_with_ledger_info(ledger_info);

        let network = Arc::new(network);

        let payload_client = Arc::new(MockPayloadManager::new(None));
        let payload_manager = Arc::new(PayloadManager::DirectMempool);

        let state_computer = Arc::new(EmptyStateComputer {});

        let (nh_abort_handle, df_abort_handle, dag_rpc_tx, ordered_nodes_rx) =
            bootstrap_dag_for_test(
                self_peer,
                signer,
                Arc::new(epoch_state),
                Arc::new(dag_storage),
                network.clone(),
                network.clone(),
                network.clone(),
                time_service,
                payload_manager,
                payload_client,
                state_computer,
            );

        (
            Self {
                nh_task_handle: nh_abort_handle,
                df_task_handle: df_abort_handle,
                dag_rpc_tx,
                network_events,
            },
            ordered_nodes_rx,
        )
    }

    async fn start(mut self) {
        loop {
            match self.network_events.next().await.unwrap() {
                Event::RpcRequest(sender, msg, protocol, response_sender) => match msg {
                    ConsensusMsg::DAGMessage(msg) => {
                        debug!("handling RPC...");
                        self.dag_rpc_tx.push(sender, IncomingDAGRequest {
                            req: msg,
                            sender,
                            protocol,
                            response_sender,
                        })
                    },
                    _ => unreachable!("expected only DAG-related messages"),
                },
                _ => panic!("Unexpected Network Event"),
            }
            .unwrap()
        }
    }
}

fn create_network(
    playground: &mut NetworkPlayground,
    id: usize,
    author: Author,
    validators: ValidatorVerifier,
) -> (
    NetworkSender,
    Box<Select<NetworkEvents<ConsensusMsg>, aptos_channels::Receiver<Event<ConsensusMsg>>>>,
) {
    let (network_reqs_tx, network_reqs_rx) = aptos_channel::new(QueueStyle::FIFO, 8, None);
    let (connection_reqs_tx, _) = aptos_channel::new(QueueStyle::FIFO, 8, None);
    let (consensus_tx, consensus_rx) = aptos_channel::new(QueueStyle::FIFO, 8, None);
    let (_conn_mgr_reqs_tx, conn_mgr_reqs_rx) = aptos_channels::new_test(8);
    let (_, conn_status_rx) = conn_notifs_channel::new();
    let network_sender = network::NetworkSender::new(
        PeerManagerRequestSender::new(network_reqs_tx),
        ConnectionRequestSender::new(connection_reqs_tx),
    );
    let network_client = NetworkClient::new(
        DIRECT_SEND.into(),
        RPC.into(),
        hashmap! {NetworkId::Validator => network_sender},
        playground.peer_protocols(),
    );
    let consensus_network_client = ConsensusNetworkClient::new(network_client);
    let network_events = NetworkEvents::new(consensus_rx, conn_status_rx, None);

    let (self_sender, self_receiver) = aptos_channels::new_test(1000);
    let network = NetworkSender::new(author, consensus_network_client, self_sender, validators);

    let twin_id = TwinId { id, author };

    playground.add_node(twin_id, consensus_tx, network_reqs_rx, conn_mgr_reqs_rx);

    let all_network_events = Box::new(select(network_events, self_receiver));

    (network, all_network_events)
}

fn bootstrap_nodes(
    playground: &mut NetworkPlayground,
    signers: Vec<ValidatorSigner>,
    validators: ValidatorVerifier,
) -> (Vec<DagBootstrapUnit>, Vec<UnboundedReceiver<OrderedBlocks>>) {
    let peers_and_metadata = playground.peer_protocols();
    let (nodes, ordered_node_receivers) = signers
        .iter()
        .enumerate()
        .map(|(id, signer)| {
            let peer_id = signer.author();
            let mut conn_meta = ConnectionMetadata::mock(peer_id);
            conn_meta.application_protocols = ProtocolIdSet::from_iter([
                ProtocolId::ConsensusDirectSendJson,
                ProtocolId::ConsensusDirectSendBcs,
                ProtocolId::ConsensusRpcBcs,
            ]);
            let peer_network_id = PeerNetworkId::new(NetworkId::Validator, peer_id);
            peers_and_metadata
                .insert_connection_metadata(peer_network_id, conn_meta)
                .unwrap();

            let (_, storage) = MockStorage::start_for_testing((&validators).into());
            let (network, network_events) =
                create_network(playground, id, signer.author(), validators.clone());

            DagBootstrapUnit::make(
                signer.author(),
                1,
                signer.clone(),
                storage,
                network,
                aptos_time_service::TimeService::real(),
                network_events,
                signers.clone(),
            )
        })
        .unzip();

    (nodes, ordered_node_receivers)
}

#[tokio::test]
async fn test_dag_e2e() {
    let num_nodes = 7;
    let runtime = consensus_runtime();
    let mut playground = NetworkPlayground::new(runtime.handle().clone());
    let (signers, validators) = random_validator_verifier(num_nodes, None, false);

    let (nodes, mut ordered_node_receivers) = bootstrap_nodes(&mut playground, signers, validators);
    for node in nodes {
        runtime.spawn(node.start());
    }

    runtime.spawn(playground.start());

    for _ in 1..10 {
        let mut all_ordered = vec![];
        for receiver in &mut ordered_node_receivers {
            let block = receiver.next().await.unwrap();
            all_ordered.push(block.ordered_blocks)
        }
        let first = all_ordered.first().unwrap();
        assert_gt!(first.len(), 0, "must order nodes");
        debug!("Nodes: {:?}", first);
        for a in all_ordered.iter() {
            assert_eq!(a.len(), first.len(), "length should match");
            assert_eq!(a, first);
        }
    }
    runtime.shutdown_background();
}
