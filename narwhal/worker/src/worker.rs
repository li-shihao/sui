// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use crate::{
    batch_maker::BatchMaker, helper::Helper, metrics::WorkerChannelMetrics,
    primary_connector::PrimaryConnector, processor::Processor, quorum_waiter::QuorumWaiter,
    synchronizer::Synchronizer,
};
use async_trait::async_trait;
use bytes::Bytes;
use config::{Parameters, SharedCommittee, SharedWorkerCache, WorkerId};
use crypto::{NetworkKeyPair, PublicKey};
use futures::{Stream, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use network::{metrics::WorkerNetworkMetrics, WorkerNetwork};
use primary::PrimaryWorkerMessage;
use std::{net::Ipv4Addr, pin::Pin, sync::Arc};
use store::Store;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tonic::{Request, Response, Status};
use tracing::info;
use types::{
    error::DagError,
    metered_channel::{channel, Sender},
    BatchDigest, BincodeEncodedPayload, ClientBatchRequest, Empty, PrimaryToWorker,
    PrimaryToWorkerServer, ReconfigureNotification, SerializedBatchMessage, Transaction,
    TransactionProto, Transactions, TransactionsServer, WorkerPrimaryMessage, WorkerToWorker,
    WorkerToWorkerServer,
};

#[cfg(test)]
#[path = "tests/worker_tests.rs"]
pub mod worker_tests;

/// The default channel capacity for each channel of the worker.
pub const CHANNEL_CAPACITY: usize = 1_000;

use crate::metrics::{Metrics, WorkerEndpointMetrics, WorkerMetrics};
pub use types::WorkerMessage;

pub struct Worker {
    /// The public key of this authority.
    primary_name: PublicKey,
    // The private-public key pair of this worker.
    // TODO: utilize keypair in network communication
    #[allow(dead_code)]
    keypair: NetworkKeyPair,
    /// The id of this worker used for index-based lookup by other NW nodes.
    id: WorkerId,
    /// The committee information.
    committee: SharedCommittee,
    /// The worker information cache.
    worker_cache: SharedWorkerCache,
    /// The configuration parameters
    parameters: Parameters,
    /// The persistent storage.
    store: Store<BatchDigest, SerializedBatchMessage>,
}

const INADDR_ANY: Ipv4Addr = Ipv4Addr::new(0, 0, 0, 0);

impl Worker {
    pub fn spawn(
        primary_name: PublicKey,
        keypair: NetworkKeyPair,
        id: WorkerId,
        committee: SharedCommittee,
        worker_cache: SharedWorkerCache,
        parameters: Parameters,
        store: Store<BatchDigest, SerializedBatchMessage>,
        metrics: Metrics,
    ) -> Vec<JoinHandle<()>> {
        // Define a worker instance.
        let worker = Self {
            primary_name: primary_name.clone(),
            keypair,
            id,
            committee: committee.clone(),
            worker_cache,
            parameters,
            store,
        };

        let node_metrics = Arc::new(metrics.worker_metrics.unwrap());
        let endpoint_metrics = metrics.endpoint_metrics.unwrap();
        let network_metrics = Arc::new(metrics.network_metrics.unwrap());
        let channel_metrics: Arc<WorkerChannelMetrics> = Arc::new(metrics.channel_metrics.unwrap());

        // Spawn all worker tasks.
        let (tx_primary, rx_primary) = channel(CHANNEL_CAPACITY, &channel_metrics.tx_primary);

        let initial_committee = (*(*(*committee).load()).clone()).clone();
        let (tx_reconfigure, rx_reconfigure) =
            watch::channel(ReconfigureNotification::NewEpoch(initial_committee.clone()));

        let client_flow_handles = worker.handle_clients_transactions(
            &tx_reconfigure,
            tx_primary.clone(),
            node_metrics.clone(),
            channel_metrics.clone(),
            endpoint_metrics,
            network_metrics.clone(),
        );
        let worker_flow_handles = worker.handle_workers_messages(
            &tx_reconfigure,
            tx_primary.clone(),
            channel_metrics.clone(),
            network_metrics.clone(),
        );
        let primary_flow_handles = worker.handle_primary_messages(
            tx_reconfigure,
            tx_primary,
            node_metrics,
            channel_metrics,
            network_metrics,
        );

        // The `PrimaryConnector` allows the worker to send messages to its primary.
        let handle =
            PrimaryConnector::spawn(primary_name, initial_committee, rx_reconfigure, rx_primary);

        // NOTE: This log entry is used to compute performance.
        info!(
            "Worker {} successfully booted on {}",
            id,
            worker
                .worker_cache
                .load()
                .worker(&worker.primary_name, &worker.id)
                .expect("Our public key or worker id is not in the worker cache")
                .transactions
        );

        let mut handles = vec![handle];
        handles.extend(primary_flow_handles);
        handles.extend(client_flow_handles);
        handles.extend(worker_flow_handles);
        handles
    }

    /// Spawn all tasks responsible to handle messages from our primary.
    fn handle_primary_messages(
        &self,
        tx_reconfigure: watch::Sender<ReconfigureNotification>,
        tx_primary: Sender<WorkerPrimaryMessage>,
        node_metrics: Arc<WorkerMetrics>,
        channel_metrics: Arc<WorkerChannelMetrics>,
        network_metrics: Arc<WorkerNetworkMetrics>,
    ) -> Vec<JoinHandle<()>> {
        let (tx_synchronizer, rx_synchronizer) =
            channel(CHANNEL_CAPACITY, &channel_metrics.tx_synchronizer);

        // Receive incoming messages from our primary.
        let address = self
            .worker_cache
            .load()
            .worker(&self.primary_name, &self.id)
            .expect("Our public key or worker id is not in the worker cache")
            .primary_to_worker;
        let address = address
            .replace(0, |_protocol| Some(Protocol::Ip4(INADDR_ANY)))
            .unwrap();
        let primary_handle = PrimaryReceiverHandler { tx_synchronizer }
            .spawn(address.clone(), tx_reconfigure.subscribe());

        // The `Synchronizer` is responsible to keep the worker in sync with the others. It handles the commands
        // it receives from the primary (which are mainly notifications that we are out of sync).
        let worker_network = WorkerNetwork::new(network::metrics::Metrics::new(
            network_metrics,
            "synchronizer".to_string(),
        ));
        let handle = Synchronizer::spawn(
            self.primary_name.clone(),
            self.id,
            self.committee.clone(),
            self.worker_cache.clone(),
            self.store.clone(),
            self.parameters.gc_depth,
            self.parameters.sync_retry_delay,
            self.parameters.sync_retry_nodes,
            /* rx_message */ rx_synchronizer,
            tx_reconfigure,
            tx_primary,
            node_metrics,
            worker_network,
        );

        info!(
            "Worker {} listening to primary messages on {}",
            self.id, address
        );

        vec![handle, primary_handle]
    }

    /// Spawn all tasks responsible to handle clients transactions.
    fn handle_clients_transactions(
        &self,
        tx_reconfigure: &watch::Sender<ReconfigureNotification>,
        tx_primary: Sender<WorkerPrimaryMessage>,
        node_metrics: Arc<WorkerMetrics>,
        channel_metrics: Arc<WorkerChannelMetrics>,
        endpoint_metrics: WorkerEndpointMetrics,
        network_metrics: Arc<WorkerNetworkMetrics>,
    ) -> Vec<JoinHandle<()>> {
        let (tx_batch_maker, rx_batch_maker) =
            channel(CHANNEL_CAPACITY, &channel_metrics.tx_batch_maker);
        let (tx_quorum_waiter, rx_quorum_waiter) =
            channel(CHANNEL_CAPACITY, &channel_metrics.tx_quorum_waiter);
        let (tx_client_processor, rx_client_processor) =
            channel(CHANNEL_CAPACITY, &channel_metrics.tx_client_processor);

        // We first receive clients' transactions from the network.
        let address = self
            .worker_cache
            .load()
            .worker(&self.primary_name, &self.id)
            .expect("Our public key or worker id is not in the worker cache")
            .transactions;
        let address = address
            .replace(0, |_protocol| Some(Protocol::Ip4(INADDR_ANY)))
            .unwrap();
        let tx_receiver_handle = TxReceiverHandler { tx_batch_maker }.spawn(
            address.clone(),
            tx_reconfigure.subscribe(),
            endpoint_metrics,
        );

        // The transactions are sent to the `BatchMaker` that assembles them into batches. It then broadcasts
        // (in a reliable manner) the batches to all other workers that share the same `id` as us. Finally, it
        // gathers the 'cancel handlers' of the messages and send them to the `QuorumWaiter`.
        let batch_maker_handle = BatchMaker::spawn(
            (*(*(*self.committee).load()).clone()).clone(),
            self.parameters.batch_size,
            self.parameters.max_batch_delay,
            tx_reconfigure.subscribe(),
            /* rx_transaction */ rx_batch_maker,
            /* tx_message */ tx_quorum_waiter,
            node_metrics,
        );

        // The `QuorumWaiter` waits for 2f authorities to acknowledge reception of the batch. It then forwards
        // the batch to the `Processor`.
        let worker_network = WorkerNetwork::new(network::metrics::Metrics::new(
            network_metrics,
            "quorum_waiter".to_string(),
        ));
        let quorum_waiter_handle = QuorumWaiter::spawn(
            self.primary_name.clone(),
            self.id,
            (*(*(*self.committee).load()).clone()).clone(),
            self.worker_cache.clone(),
            tx_reconfigure.subscribe(),
            /* rx_message */ rx_quorum_waiter,
            /* tx_batch */ tx_client_processor,
            worker_network,
        );

        // The `Processor` hashes and stores the batch. It then forwards the batch's digest to the `PrimaryConnector`
        // that will send it to our primary machine.
        let processor_handle = Processor::spawn(
            self.id,
            self.store.clone(),
            tx_reconfigure.subscribe(),
            /* rx_batch */ rx_client_processor,
            /* tx_digest */ tx_primary,
            /* own_batch */ true,
        );

        info!(
            "Worker {} listening to client transactions on {}",
            self.id, address
        );

        vec![
            batch_maker_handle,
            quorum_waiter_handle,
            processor_handle,
            tx_receiver_handle,
        ]
    }

    /// Spawn all tasks responsible to handle messages from other workers.
    fn handle_workers_messages(
        &self,
        tx_reconfigure: &watch::Sender<ReconfigureNotification>,
        tx_primary: Sender<WorkerPrimaryMessage>,
        channel_metrics: Arc<WorkerChannelMetrics>,
        network_metrics: Arc<WorkerNetworkMetrics>,
    ) -> Vec<JoinHandle<()>> {
        let (tx_worker_helper, rx_worker_helper) =
            channel(CHANNEL_CAPACITY, &channel_metrics.tx_worker_helper);
        let (tx_client_helper, rx_client_helper) =
            channel(CHANNEL_CAPACITY, &channel_metrics.tx_client_helper);
        let (tx_worker_processor, rx_worker_processor) =
            channel(CHANNEL_CAPACITY, &channel_metrics.tx_worker_processor);

        // Receive incoming messages from other workers.
        let address = self
            .worker_cache
            .load()
            .worker(&self.primary_name, &self.id)
            .expect("Our public key or worker id is not in the worker cache")
            .worker_to_worker;
        let address = address
            .replace(0, |_protocol| Some(Protocol::Ip4(INADDR_ANY)))
            .unwrap();
        let worker_handle = WorkerReceiverHandler {
            tx_worker_helper,
            tx_client_helper,
            tx_processor: tx_worker_processor,
        }
        .spawn(
            address.clone(),
            self.parameters.max_concurrent_requests,
            tx_reconfigure.subscribe(),
        );

        // The `Helper` is dedicated to reply to batch requests from other workers.
        let worker_network = WorkerNetwork::new(network::metrics::Metrics::new(
            network_metrics,
            "worker_helper".to_string(),
        ));
        let helper_handle = Helper::spawn(
            self.id,
            (*(*(*self.committee).load()).clone()).clone(),
            self.worker_cache.clone(),
            self.store.clone(),
            tx_reconfigure.subscribe(),
            /* rx_worker_request */ rx_worker_helper,
            /* rx_client_request */ rx_client_helper,
            worker_network,
        );

        // This `Processor` hashes and stores the batches we receive from the other workers. It then forwards the
        // batch's digest to the `PrimaryConnector` that will send it to our primary.
        let processor_handle = Processor::spawn(
            self.id,
            self.store.clone(),
            tx_reconfigure.subscribe(),
            /* rx_batch */ rx_worker_processor,
            /* tx_digest */ tx_primary,
            /* own_batch */ false,
        );

        info!(
            "Worker {} listening to worker messages on {}",
            self.id, address
        );

        vec![helper_handle, processor_handle, worker_handle]
    }
}

/// Defines how the network receiver handles incoming transactions.
#[derive(Clone)]
struct TxReceiverHandler {
    tx_batch_maker: Sender<Transaction>,
}

impl TxReceiverHandler {
    async fn wait_for_shutdown(mut rx_reconfigure: watch::Receiver<ReconfigureNotification>) {
        loop {
            let result = rx_reconfigure.changed().await;
            result.expect("Committee channel dropped");
            let message = rx_reconfigure.borrow().clone();
            if let ReconfigureNotification::Shutdown = message {
                break;
            }
        }
    }

    #[must_use]
    fn spawn(
        self,
        address: Multiaddr,
        rx_reconfigure: watch::Receiver<ReconfigureNotification>,
        endpoint_metrics: WorkerEndpointMetrics,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            tokio::select! {
                _result =  mysten_network::config::Config::new()
                    .server_builder_with_metrics(endpoint_metrics)
                    .add_service(TransactionsServer::new(self))
                    .bind(&address)
                    .await
                    .unwrap()
                    .serve() => (),

                () = Self::wait_for_shutdown(rx_reconfigure) => ()
            }
        })
    }
}

#[async_trait]
impl Transactions for TxReceiverHandler {
    async fn submit_transaction(
        &self,
        request: Request<TransactionProto>,
    ) -> Result<Response<Empty>, Status> {
        let message = request.into_inner().transaction;
        // Send the transaction to the batch maker.
        self.tx_batch_maker
            .send(message.to_vec())
            .await
            .map_err(|_| DagError::ShuttingDown)
            .map_err(|e| Status::not_found(e.to_string()))?;

        Ok(Response::new(Empty {}))
    }

    async fn submit_transaction_stream(
        &self,
        request: tonic::Request<tonic::Streaming<types::TransactionProto>>,
    ) -> Result<tonic::Response<types::Empty>, tonic::Status> {
        let mut transactions = request.into_inner();

        while let Some(Ok(txn)) = transactions.next().await {
            // Send the transaction to the batch maker.
            self.tx_batch_maker
                .send(txn.transaction.to_vec())
                .await
                .expect("Failed to send transaction");
        }
        Ok(Response::new(Empty {}))
    }
}

/// Defines how the network receiver handles incoming workers messages.
#[derive(Clone)]
struct WorkerReceiverHandler {
    tx_worker_helper: Sender<(Vec<BatchDigest>, PublicKey)>,
    tx_client_helper: Sender<(Vec<BatchDigest>, mpsc::Sender<SerializedBatchMessage>)>,
    tx_processor: Sender<SerializedBatchMessage>,
}

impl WorkerReceiverHandler {
    async fn wait_for_shutdown(mut rx_reconfigure: watch::Receiver<ReconfigureNotification>) {
        loop {
            let result = rx_reconfigure.changed().await;
            result.expect("Committee channel dropped");
            let message = rx_reconfigure.borrow().clone();
            if let ReconfigureNotification::Shutdown = message {
                break;
            }
        }
    }

    #[must_use]
    fn spawn(
        self,
        address: Multiaddr,
        max_concurrent_requests: usize,
        rx_reconfigure: watch::Receiver<ReconfigureNotification>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut config = mysten_network::config::Config::new();
            config.concurrency_limit_per_connection = Some(max_concurrent_requests);
            tokio::select! {
                _result = config
                .server_builder()
                .add_service(WorkerToWorkerServer::new(self))
                .bind(&address)
                .await
                .unwrap()
                .serve() => (),

                () = Self::wait_for_shutdown(rx_reconfigure) => ()
            }
        })
    }
}

#[async_trait]
impl WorkerToWorker for WorkerReceiverHandler {
    async fn send_message(
        &self,
        request: Request<BincodeEncodedPayload>,
    ) -> Result<Response<Empty>, Status> {
        let message: WorkerMessage = request
            .get_ref()
            .deserialize()
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        match message {
            WorkerMessage::Batch(..) => self
                .tx_processor
                .send(request.get_ref().payload.to_vec())
                .await
                .map_err(|_| DagError::ShuttingDown),

            WorkerMessage::BatchRequest(missing, requestor) => self
                .tx_worker_helper
                .send((missing, requestor))
                .await
                .map_err(|_| DagError::ShuttingDown),
        }
        .map_err(|e| Status::not_found(e.to_string()))?;

        Ok(Response::new(Empty {}))
    }

    type ClientBatchRequestStream =
        Pin<Box<dyn Stream<Item = Result<BincodeEncodedPayload, Status>> + Send>>;

    async fn client_batch_request(
        &self,
        request: Request<BincodeEncodedPayload>,
    ) -> Result<Response<Self::ClientBatchRequestStream>, Status> {
        let missing = request
            .into_inner()
            .deserialize::<ClientBatchRequest>()
            .map_err(|e| Status::invalid_argument(e.to_string()))?
            .0;

        // TODO [issue #7]: Do some accounting to prevent bad actors from use all our
        // resources (in this case allocate a gigantic channel).
        let (sender, receiver) = mpsc::channel(missing.len());

        self.tx_client_helper
            .send((missing, sender))
            .await
            .map_err(|_| DagError::ShuttingDown)
            .map_err(|e| Status::not_found(e.to_string()))?;

        let stream = tokio_stream::wrappers::ReceiverStream::new(receiver).map(|batch| {
            let payload = BincodeEncodedPayload {
                payload: Bytes::from(batch),
            };
            Ok(payload)
        });

        Ok(Response::new(
            Box::pin(stream) as Self::ClientBatchRequestStream
        ))
    }
}

/// Defines how the network receiver handles incoming primary messages.
#[derive(Clone)]
struct PrimaryReceiverHandler {
    tx_synchronizer: Sender<PrimaryWorkerMessage>,
}

impl PrimaryReceiverHandler {
    async fn wait_for_shutdown(mut rx_reconfigure: watch::Receiver<ReconfigureNotification>) {
        loop {
            let result = rx_reconfigure.changed().await;
            result.expect("Committee channel dropped");
            let message = rx_reconfigure.borrow().clone();
            if let ReconfigureNotification::Shutdown = message {
                break;
            }
        }
    }

    #[must_use]
    fn spawn(
        self,
        address: Multiaddr,
        rx_reconfigure: watch::Receiver<ReconfigureNotification>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            tokio::select! {
                _result = mysten_network::config::Config::new()
                    .server_builder()
                    .add_service(PrimaryToWorkerServer::new(self))
                    .bind(&address)
                    .await
                    .unwrap()
                    .serve() => (),

                () = Self::wait_for_shutdown(rx_reconfigure) => ()
            }
        })
    }
}

#[async_trait]
impl PrimaryToWorker for PrimaryReceiverHandler {
    async fn send_message(
        &self,
        request: Request<BincodeEncodedPayload>,
    ) -> Result<Response<Empty>, Status> {
        let message: PrimaryWorkerMessage = request
            .into_inner()
            .deserialize()
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        self.tx_synchronizer
            .send(message)
            .await
            .map_err(|_| DagError::ShuttingDown)
            .map_err(|e| Status::not_found(e.to_string()))?;

        Ok(Response::new(Empty {}))
    }
}