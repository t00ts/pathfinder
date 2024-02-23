//! _High level_ client for p2p interaction.
//! Frees the caller from managing peers manually.
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;
use libp2p::PeerId;
use p2p_proto::{
    common::{Direction, Iteration},
    transaction::TransactionsRequest,
};
use p2p_proto::{
    header::{BlockHeadersRequest, BlockHeadersResponse},
    transaction::TransactionsResponse,
};
use pathfinder_common::{transaction::Transaction, BlockNumber, SignedBlockHeader};
use tokio::sync::RwLock;

use crate::client::{conv::TryFromDto, peer_aware};
use crate::sync::protocol;

/// Data received from a specific peer.
#[derive(Debug)]
pub struct PeerData<T> {
    pub peer: PeerId,
    pub data: T,
}

impl<T> PeerData<T> {
    pub fn new(peer: PeerId, data: T) -> Self {
        Self { peer, data }
    }
}

#[derive(Clone, Debug)]
pub struct Client {
    inner: peer_aware::Client,
    block_propagation_topic: String,
    peers_with_capability: Arc<RwLock<PeersWithCapability>>,
}

// TODO Rework the API!
// I.e. make sure the api looks reasonable from the perspective of
// the __user__, which is the sync driving algo/entity.
impl Client {
    pub fn new(inner: peer_aware::Client, block_propagation_topic: String) -> Self {
        Self {
            inner,
            block_propagation_topic,
            peers_with_capability: Default::default(),
        }
    }

    // Propagate new L2 head head
    pub async fn propagate_new_head(
        &self,
        block_id: p2p_proto::common::BlockId,
    ) -> anyhow::Result<()> {
        tracing::debug!(number=%block_id.number, hash=%block_id.hash.0, topic=%self.block_propagation_topic,
            "Propagating head"
        );

        self.inner
            .publish(
                &self.block_propagation_topic,
                p2p_proto::header::NewBlock::Id(block_id),
            )
            .await
    }

    async fn get_update_peers_with_sync_capability(&self, capability: &str) -> Vec<PeerId> {
        use rand::seq::SliceRandom;

        let r = self.peers_with_capability.read().await;
        let mut peers = if let Some(peers) = r.get(capability) {
            peers.iter().copied().collect::<Vec<_>>()
        } else {
            // Avoid deadlock
            drop(r);

            let mut peers = self
                .inner
                .get_capability_providers(capability)
                .await
                .unwrap_or_default();

            let _i_should_have_the_capability_too = peers.remove(self.inner.peer_id());
            debug_assert!(_i_should_have_the_capability_too);

            let peers_vec = peers.iter().copied().collect::<Vec<_>>();

            let mut w = self.peers_with_capability.write().await;
            w.update(capability, peers);
            peers_vec
        };
        peers.shuffle(&mut rand::thread_rng());
        peers
    }

    pub fn header_stream(
        self,
        start: BlockNumber,
        stop: BlockNumber,
        reverse: bool,
    ) -> impl futures::Stream<Item = PeerData<SignedBlockHeader>> {
        let (mut start, stop, direction) = match reverse {
            true => (stop, start, Direction::Backward),
            false => (start, stop, Direction::Forward),
        };

        async_stream::stream! {
            // Loop which refreshes peer set once we exhaust it.
            loop {
                let peers = self
                    .get_update_peers_with_sync_capability(protocol::Headers::NAME)
                    .await;

                // Attempt each peer.
                'next_peer: for peer in peers {
                    let limit = start.get().max(stop.get()) - start.get().min(stop.get());

                    let request = BlockHeadersRequest {
                        iteration: Iteration {
                            start: start.get().into(),
                            direction,
                            limit,
                            step: 1.into(),
                        },
                    };

                    let mut responses = match self.inner.send_headers_sync_request(peer, request).await
                    {
                        Ok(x) => x,
                        Err(error) => {
                            // Failed to establish connection, try next peer.
                            tracing::debug!(%peer, reason=%error, "Headers request failed");
                            continue 'next_peer;
                        }
                    };

                    while let Some(signed_header) = responses.next().await {
                        let signed_header = match signed_header {
                            BlockHeadersResponse::Header(hdr) =>
                            match SignedBlockHeader::try_from_dto(*hdr) {
                                Ok(hdr) => hdr,
                                Err(error) => {
                                    tracing::debug!(%peer, %error, "Header stream failed");
                                    continue 'next_peer;
                                },
                            },
                            BlockHeadersResponse::Fin => {
                                tracing::debug!(%peer, "Header stream Fin");
                                continue 'next_peer;
                            }
                        };

                        start = match direction {
                            Direction::Forward => start + 1,
                            // unwrap_or_default is safe as this is the genesis edge case,
                            // at which point the loop will complete at the end of this iteration.
                            Direction::Backward => start.parent().unwrap_or_default(),
                        };

                        yield PeerData::new(peer, signed_header);
                    }

                    // TODO: track how much and how fast this peer responded with i.e. don't let them drip feed us etc.
                }
            }
        }
    }

    // TODO I'm now realizing that in order to do anything useful, we have to get all transactions
    // for a block. So this should really return Vec<Transaction> and I don't really need to fetch
    // transaction counts from the DB because the transaction count should be either zero OR there
    // all the transacations should be present (i.e. the transaction update should happen in a
    // commitment which includes all the transactions).
    pub fn transaction_stream(
        self,
        block: BlockNumber,
    ) -> impl futures::Stream<Item = PeerData<Vec<Transaction>>> {
        async_stream::stream! {
            // Loop which refreshes peer set once we exhaust it.
            loop {
                let peers = self
                    .get_update_peers_with_sync_capability(protocol::Transactions::NAME)
                    .await;

                // Attempt each peer.
                'next_peer: for peer in peers {
                    let request = TransactionsRequest {
                        iteration: Iteration {
                            start: block.get().into(),
                            direction: Direction::Forward,
                            limit: 1,
                            step: 1.into(),
                        },
                    };

                    let mut responses = match self.inner.send_transactions_sync_request(peer, request).await
                    {
                        Ok(x) => x,
                        Err(error) => {
                            // Failed to establish connection, try next peer.
                            tracing::debug!(%peer, reason=%error, "Transactions request failed");
                            continue 'next_peer;
                        }
                    };

                    let mut transactions = Vec::new();
                    while let Some(transaction) = responses.next().await {
                        match transaction {
                            TransactionsResponse::Transaction(tx) => match Transaction::try_from_dto(tx.variant) {
                                Ok(tx) => transactions.push(tx),
                                Err(error) => {
                                    tracing::debug!(%peer, %error, "Transaction stream failed");
                                    continue 'next_peer;
                                },
                            },
                            TransactionsResponse::Fin => {
                                tracing::debug!(%peer, "Transaction stream Fin");
                                yield PeerData::new(peer, transactions);
                                continue 'next_peer;
                            }
                        };
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct PeersWithCapability {
    set: HashMap<String, HashSet<PeerId>>,
    last_update: std::time::Instant,
    timeout: Duration,
}

impl PeersWithCapability {
    pub fn new(timeout: Duration) -> Self {
        Self {
            set: Default::default(),
            last_update: std::time::Instant::now(),
            timeout,
        }
    }

    /// Does not clear if elapsed, instead the caller is expected to call [`Self::update`]
    pub fn get(&self, capability: &str) -> Option<&HashSet<PeerId>> {
        if self.last_update.elapsed() > self.timeout {
            None
        } else {
            self.set.get(capability)
        }
    }

    pub fn update(&mut self, capability: &str, peers: HashSet<PeerId>) {
        self.last_update = std::time::Instant::now();
        self.set.insert(capability.to_owned(), peers);
    }
}

impl Default for PeersWithCapability {
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}
