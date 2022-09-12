// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use super::*;
use store::rocks;
use test_utils::{
    batch, digest_batch, serialize_batch_message, temp_dir, CommitteeFixture,
    WorkerToWorkerMockServer,
};
use tokio::sync::mpsc::channel;
use types::BatchDigest;

#[tokio::test]
async fn worker_batch_reply() {
    let (tx_worker_request, rx_worker_request) = test_utils::test_channel!(1);
    let (_tx_client_request, rx_client_request) = test_utils::test_channel!(1);
    let fixture = CommitteeFixture::builder().randomize_ports(true).build();
    let committee = fixture.committee();
    let worker_cache = fixture.shared_worker_cache();
    let requestor = fixture.authorities().next().unwrap().public_key();
    let id = 0;
    let (_tx_reconfiguration, rx_reconfiguration) =
        watch::channel(ReconfigureNotification::NewEpoch(committee.clone()));

    // Create a new test store.
    let db = rocks::DBMap::<BatchDigest, SerializedBatchMessage>::open(
        temp_dir(),
        None,
        Some("batches"),
    )
    .unwrap();
    let store = Store::new(db);

    // Add a batch to the store.
    let batch = batch();
    let serialized_batch = serialize_batch_message(batch.clone());
    let batch_digest = digest_batch(batch.clone());
    store.write(batch_digest, serialized_batch.clone()).await;

    // Spawn an `Helper` instance.
    let _helper_handle = Helper::spawn(
        id,
        committee.clone(),
        worker_cache.clone(),
        store,
        rx_reconfiguration,
        rx_worker_request,
        rx_client_request,
        WorkerNetwork::default(),
    );

    // Spawn a listener to receive the batch reply.
    let address = worker_cache
        .load()
        .worker(&requestor, &id)
        .unwrap()
        .worker_to_worker;
    let expected = Bytes::from(serialized_batch.clone());
    let mut handle = WorkerToWorkerMockServer::spawn(address);

    // Send a batch request.
    let digests = vec![batch_digest];
    tx_worker_request.send((digests, requestor)).await.unwrap();

    // Ensure the requestor received the batch (ie. it did not panic).
    assert_eq!(handle.recv().await.unwrap().payload, expected);
}

#[tokio::test]
async fn client_batch_reply() {
    let (_tx_worker_request, rx_worker_request) = test_utils::test_channel!(1);
    let (tx_client_request, rx_client_request) = test_utils::test_channel!(1);
    let id = 0;
    let fixture = CommitteeFixture::builder().randomize_ports(true).build();
    let committee = fixture.committee();
    let worker_cache = fixture.shared_worker_cache();
    let (_tx_reconfiguration, rx_reconfiguration) =
        watch::channel(ReconfigureNotification::NewEpoch(committee.clone()));

    // Create a new test store.
    let db = rocks::DBMap::<BatchDigest, SerializedBatchMessage>::open(
        temp_dir(),
        None,
        Some("batches"),
    )
    .unwrap();
    let store = Store::new(db);

    // Add a batch to the store.
    let batch = batch();
    let serialized_batch = serialize_batch_message(batch.clone());
    let batch_digest = digest_batch(batch.clone());
    store.write(batch_digest, serialized_batch.clone()).await;

    // Spawn an `Helper` instance.
    let _helper_handler = Helper::spawn(
        id,
        committee.clone(),
        worker_cache.clone(),
        store,
        rx_reconfiguration,
        rx_worker_request,
        rx_client_request,
        WorkerNetwork::default(),
    );

    // Send batch request.
    let digests = vec![batch_digest];
    let (sender, mut receiver) = channel(digests.len());
    tx_client_request.send((digests, sender)).await.unwrap();

    // Wait for the reply and ensure it is as expected.
    while let Some(bytes) = receiver.recv().await {
        assert_eq!(bytes, serialized_batch);
    }
}