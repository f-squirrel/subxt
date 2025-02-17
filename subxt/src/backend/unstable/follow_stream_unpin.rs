// Copyright 2019-2023 Parity Technologies (UK) Ltd.
// This file is dual-licensed as Apache-2.0 or GPL-3.0.
// see LICENSE for license details.

use super::follow_stream::FollowStream;
use super::UnstableRpcMethods;
use crate::backend::unstable::rpc_methods::{
    BestBlockChanged, Finalized, FollowEvent, Initialized, NewBlock,
};
use crate::config::{BlockHash, Config};
use crate::error::Error;
use futures::stream::{FuturesUnordered, Stream, StreamExt};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// The type of stream item.
pub use super::follow_stream::FollowStreamMsg;

/// A `Stream` which builds on `FollowStream`, and handles pinning. It replaces any block hash seen in
/// the follow events with a `BlockRef` which, when all clones are dropped, will lead to an "unpin" call
/// for that block hash being queued. It will also automatically unpin any blocks that exceed a given max
/// age, to try and prevent the underlying stream from ending (and _all_ blocks from being unpinned as a
/// result). Put simply, it tries to keep every block pinned as long as possible until the block is no longer
/// used anywhere.
#[derive(Debug)]
pub struct FollowStreamUnpin<Hash: BlockHash> {
    // The underlying stream of events.
    inner: FollowStream<Hash>,
    // A method to call to unpin a block, given a block hash and a subscription ID.
    unpin_method: UnpinMethodHolder<Hash>,
    // Futures for sending unpin events that we'll poll to completion as
    // part of polling the stream as a whole.
    unpin_futs: FuturesUnordered<UnpinFut>,
    // Each new finalized block increments this. Allows us to track
    // the age of blocks so that we can unpin old ones.
    rel_block_num: usize,
    // The latest ID of the FollowStream subscription, which we can use
    // to unpin blocks.
    subscription_id: Option<Arc<str>>,
    // The longest period a block can be pinned for.
    max_block_life: usize,
    // The currently seen and pinned blocks.
    pinned: HashMap<Hash, PinnedDetails<Hash>>,
    // Shared state about blocks we've flagged to unpin from elsewhere
    unpin_flags: UnpinFlags<Hash>,
}

// Just a wrapper to make implementing debug on the whole thing easier.
struct UnpinMethodHolder<Hash>(UnpinMethod<Hash>);
impl<Hash> std::fmt::Debug for UnpinMethodHolder<Hash> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "UnpinMethodHolder(Box<dyn FnMut(Hash, Arc<str>) -> UnpinFut>)"
        )
    }
}

/// The type of the unpin method that we need to provide.
pub type UnpinMethod<Hash> = Box<dyn FnMut(Hash, Arc<str>) -> UnpinFut + Send>;

/// The future returned from [`UnpinMethod`].
pub type UnpinFut = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

impl<Hash: BlockHash> std::marker::Unpin for FollowStreamUnpin<Hash> {}

impl<Hash: BlockHash> Stream for FollowStreamUnpin<Hash> {
    type Item = Result<FollowStreamMsg<BlockRef<Hash>>, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();

        loop {
            // Poll the unpin tasks while they are completing. if we get back None, then
            // no tasks in the list, and if pending, we'll be woken when we can poll again.
            if let Poll::Ready(Some(())) = this.unpin_futs.poll_next_unpin(cx) {
                continue;
            };

            // Poll the inner stream for the next event.
            let Poll::Ready(ev) = this.inner.poll_next_unpin(cx) else {
                return Poll::Pending;
            };

            // No more progress to be made if inner stream done.
            let Some(ev) = ev else {
                return Poll::Ready(None);
            };

            // Error? just return it and do nothing further.
            let ev = match ev {
                Ok(ev) => ev,
                Err(e) => {
                    return Poll::Ready(Some(Err(e)));
                }
            };

            // React to any actual FollowEvent we get back.
            let ev = match ev {
                FollowStreamMsg::Ready(subscription_id) => {
                    // update the subscription ID we'll use to unpin things.
                    this.subscription_id = Some(subscription_id.clone().into());

                    FollowStreamMsg::Ready(subscription_id)
                }
                FollowStreamMsg::Event(FollowEvent::Initialized(details)) => {
                    // The first finalized block gets the starting block_num.
                    let rel_block_num = this.rel_block_num;
                    let block_ref = this.pin_block_at(rel_block_num, details.finalized_block_hash);

                    FollowStreamMsg::Event(FollowEvent::Initialized(Initialized {
                        finalized_block_hash: block_ref,
                        finalized_block_runtime: details.finalized_block_runtime,
                    }))
                }
                FollowStreamMsg::Event(FollowEvent::NewBlock(details)) => {
                    // One bigger than our parent, and if no parent seen (maybe it was
                    // unpinned already), then one bigger than the last finalized block num
                    // as a best guess.
                    let parent_rel_block_num = this
                        .pinned
                        .get(&details.parent_block_hash)
                        .map(|p| p.rel_block_num)
                        .unwrap_or(this.rel_block_num);

                    let block_ref = this.pin_block_at(parent_rel_block_num + 1, details.block_hash);
                    let parent_block_ref =
                        this.pin_block_at(parent_rel_block_num, details.parent_block_hash);

                    FollowStreamMsg::Event(FollowEvent::NewBlock(NewBlock {
                        block_hash: block_ref,
                        parent_block_hash: parent_block_ref,
                        new_runtime: details.new_runtime,
                    }))
                }
                FollowStreamMsg::Event(FollowEvent::BestBlockChanged(details)) => {
                    // We expect this block to already exist, so it'll keep its existing block_num,
                    // but worst case it'll just get the current finalized block_num + 1.
                    let rel_block_num = this.rel_block_num + 1;
                    let block_ref = this.pin_block_at(rel_block_num, details.best_block_hash);

                    FollowStreamMsg::Event(FollowEvent::BestBlockChanged(BestBlockChanged {
                        best_block_hash: block_ref,
                    }))
                }
                FollowStreamMsg::Event(FollowEvent::Finalized(details)) => {
                    let finalized_block_refs: Vec<_> = details
                        .finalized_block_hashes
                        .into_iter()
                        .enumerate()
                        .map(|(idx, hash)| {
                            // These blocks _should_ exist already and so will have a known block num,
                            // but if they don't, we just increment the num from the last finalized block
                            // we saw, which should be accurate.
                            let rel_block_num = this.rel_block_num + idx + 1;
                            this.pin_block_at(rel_block_num, hash)
                        })
                        .collect();

                    // Our relative block height is increased by however many finalized
                    // blocks we've seen.
                    this.rel_block_num += finalized_block_refs.len();

                    let pruned_block_refs: Vec<_> = details
                        .pruned_block_hashes
                        .into_iter()
                        .map(|hash| {
                            // We should know about these, too, and if not we set their age to last_finalized + 1
                            let rel_block_num = this.rel_block_num + 1;
                            this.pin_block_at(rel_block_num, hash)
                        })
                        .collect();

                    // At this point, we also check to see which blocks we should submit unpin events
                    // for. When we see a block hash as finalized, we know that it won't be reported again
                    // (except as a parent hash of a new block), so we can safely make an unpin call for it
                    // without worrying about the hash being returned again despite the block not being pinned.
                    this.unpin_blocks(cx.waker());

                    FollowStreamMsg::Event(FollowEvent::Finalized(Finalized {
                        finalized_block_hashes: finalized_block_refs,
                        pruned_block_hashes: pruned_block_refs,
                    }))
                }
                FollowStreamMsg::Event(FollowEvent::Stop) => {
                    // clear out "old" things that are no longer applicable since
                    // the subscription has ended (a new one will be created under the hood).
                    this.pinned.clear();
                    this.unpin_futs.clear();
                    this.unpin_flags.lock().unwrap().clear();
                    this.rel_block_num = 0;

                    FollowStreamMsg::Event(FollowEvent::Stop)
                }
                // These events aren't intresting; we just forward them on:
                FollowStreamMsg::Event(FollowEvent::OperationBodyDone(details)) => {
                    FollowStreamMsg::Event(FollowEvent::OperationBodyDone(details))
                }
                FollowStreamMsg::Event(FollowEvent::OperationCallDone(details)) => {
                    FollowStreamMsg::Event(FollowEvent::OperationCallDone(details))
                }
                FollowStreamMsg::Event(FollowEvent::OperationStorageItems(details)) => {
                    FollowStreamMsg::Event(FollowEvent::OperationStorageItems(details))
                }
                FollowStreamMsg::Event(FollowEvent::OperationWaitingForContinue(details)) => {
                    FollowStreamMsg::Event(FollowEvent::OperationWaitingForContinue(details))
                }
                FollowStreamMsg::Event(FollowEvent::OperationStorageDone(details)) => {
                    FollowStreamMsg::Event(FollowEvent::OperationStorageDone(details))
                }
                FollowStreamMsg::Event(FollowEvent::OperationInaccessible(details)) => {
                    FollowStreamMsg::Event(FollowEvent::OperationInaccessible(details))
                }
                FollowStreamMsg::Event(FollowEvent::OperationError(details)) => {
                    FollowStreamMsg::Event(FollowEvent::OperationError(details))
                }
            };

            // Return our event.
            return Poll::Ready(Some(Ok(ev)));
        }
    }
}

impl<Hash: BlockHash> FollowStreamUnpin<Hash> {
    /// Create a new [`FollowStreamUnpin`].
    pub fn new(
        follow_stream: FollowStream<Hash>,
        unpin_method: UnpinMethod<Hash>,
        max_block_life: usize,
    ) -> Self {
        Self {
            inner: follow_stream,
            unpin_method: UnpinMethodHolder(unpin_method),
            max_block_life,
            pinned: Default::default(),
            subscription_id: None,
            rel_block_num: 0,
            unpin_flags: Default::default(),
            unpin_futs: Default::default(),
        }
    }

    /// Create a new [`FollowStreamUnpin`] given the RPC methods.
    pub fn from_methods<T: Config>(
        follow_stream: FollowStream<T::Hash>,
        methods: UnstableRpcMethods<T>,
        max_block_life: usize,
    ) -> FollowStreamUnpin<T::Hash> {
        let unpin_method = Box::new(move |hash: T::Hash, sub_id: Arc<str>| {
            let methods = methods.clone();
            let fut: UnpinFut = Box::pin(async move {
                // We ignore any errors trying to unpin at the moment.
                let _ = methods.chainhead_unstable_unpin(&sub_id, hash).await;
            });
            fut
        });

        FollowStreamUnpin::new(follow_stream, unpin_method, max_block_life)
    }

    /// Is the block hash currently pinned.
    pub fn is_pinned(&self, hash: &Hash) -> bool {
        self.pinned.contains_key(hash)
    }

    /// Pin a block, or return the reference to an already-pinned block. If the block has been registered to
    /// be unpinned, we'll clear those flags, so that it won't be unpinned. If the unpin request has already
    /// been sent though, then the block will be unpinned.
    fn pin_block_at(&mut self, rel_block_num: usize, hash: Hash) -> BlockRef<Hash> {
        let entry = self
            .pinned
            .entry(hash)
            // Only if there's already an entry do we need to clear any unpin flags set against it.
            .and_modify(|_| {
                self.unpin_flags.lock().unwrap().remove(&hash);
            })
            // If there's not an entry already, make one and return it.
            .or_insert_with(|| PinnedDetails {
                rel_block_num,
                block_ref: BlockRef {
                    inner: Arc::new(BlockRefInner {
                        hash,
                        unpin_flags: self.unpin_flags.clone(),
                    }),
                },
            });

        entry.block_ref.clone()
    }

    /// Unpin any blocks that are either too old, or have the unpin flag set and are old enough.
    fn unpin_blocks(&mut self, waker: &Waker) {
        let unpin_flags = std::mem::take(&mut *self.unpin_flags.lock().unwrap());
        let rel_block_num = self.rel_block_num;

        // If we asked to unpin and there was no subscription_id, then there's nothing to
        // do here, and we've cleared the flags now above anyway.
        let Some(sub_id) = &self.subscription_id else {
            return;
        };

        let mut blocks_to_unpin = vec![];
        for (hash, details) in &self.pinned {
            if rel_block_num.saturating_sub(details.rel_block_num) >= self.max_block_life
                || unpin_flags.contains(hash)
            {
                // The block is too old, or it's been flagged to be unpinned (no refs to it left)
                blocks_to_unpin.push(*hash);
            }
        }

        // No need to call the waker etc if nothing to do:
        if blocks_to_unpin.is_empty() {
            return;
        }

        for hash in blocks_to_unpin {
            self.pinned.remove(&hash);
            let fut = (self.unpin_method.0)(hash, sub_id.clone());
            self.unpin_futs.push(fut);
        }

        // Any new futures pushed above need polling to start. We could
        // just wait for the next stream event, but let's wake the task to
        // have it polled sooner, just incase it's slow to receive things.
        waker.wake_by_ref();
    }
}

// The set of block hashes that can be unpinned when ready.
// BlockRefs write to this when they are dropped.
type UnpinFlags<Hash> = Arc<Mutex<HashSet<Hash>>>;

#[derive(Debug)]
struct PinnedDetails<Hash: BlockHash> {
    /// How old is the block?
    rel_block_num: usize,
    /// A block ref we can hand out to keep blocks pinned.
    /// Because we store one here until it's unpinned, the live count
    /// will only drop to 1 when no external refs are left.
    block_ref: BlockRef<Hash>,
}

/// All blocks reported will be wrapped in this.
#[derive(Debug, Clone)]
pub struct BlockRef<Hash: BlockHash> {
    inner: Arc<BlockRefInner<Hash>>,
}

#[derive(Debug)]
struct BlockRefInner<Hash> {
    hash: Hash,
    unpin_flags: UnpinFlags<Hash>,
}

impl<Hash: BlockHash> BlockRef<Hash> {
    /// For testing purposes only, create a BlockRef from a hash
    /// that isn't pinned.
    #[cfg(test)]
    pub fn new(hash: Hash) -> Self {
        BlockRef {
            inner: Arc::new(BlockRefInner {
                hash,
                unpin_flags: Default::default(),
            }),
        }
    }

    /// Return the hash for this block.
    pub fn hash(&self) -> Hash {
        self.inner.hash
    }
}

impl<Hash: BlockHash> PartialEq for BlockRef<Hash> {
    fn eq(&self, other: &Self) -> bool {
        self.inner.hash == other.inner.hash
    }
}

impl<Hash: BlockHash> PartialEq<Hash> for BlockRef<Hash> {
    fn eq(&self, other: &Hash) -> bool {
        &self.inner.hash == other
    }
}

impl<Hash: BlockHash> Drop for BlockRef<Hash> {
    fn drop(&mut self) {
        // PinnedDetails keeps one ref, so if this is the second ref, it's the
        // only "external" one left and we should ask to unpin it now. if it's
        // the only ref remaining, it means that it's already been unpinned, so
        // nothing to do here anyway.
        if Arc::strong_count(&self.inner) == 2 {
            if let Ok(mut unpin_flags) = self.inner.unpin_flags.lock() {
                unpin_flags.insert(self.inner.hash);
            }
        }
    }
}

#[cfg(test)]
pub(super) mod test_utils {
    use super::super::follow_stream::{test_utils::test_stream_getter, FollowStream};
    use super::*;
    use crate::config::substrate::H256;

    pub type UnpinRx<Hash> = std::sync::mpsc::Receiver<(Hash, Arc<str>)>;

    /// Get a `FolowStreamUnpin` from an iterator over events.
    pub fn test_unpin_stream_getter<Hash, F, I>(
        events: F,
        max_life: usize,
    ) -> (FollowStreamUnpin<Hash>, UnpinRx<Hash>)
    where
        Hash: BlockHash + 'static,
        F: Fn() -> I + Send + 'static,
        I: IntoIterator<Item = Result<FollowEvent<Hash>, Error>>,
    {
        // Unpin requests will come here so that we can look out for them.
        let (unpin_tx, unpin_rx) = std::sync::mpsc::channel();

        let follow_stream = FollowStream::new(test_stream_getter(events));
        let unpin_method: UnpinMethod<Hash> = Box::new(move |hash, sub_id| {
            unpin_tx.send((hash, sub_id)).unwrap();
            Box::pin(std::future::ready(()))
        });

        let follow_unpin = FollowStreamUnpin::new(follow_stream, unpin_method, max_life);
        (follow_unpin, unpin_rx)
    }

    /// An initialized event containing a BlockRef (useful for comparisons)
    pub fn ev_initialized_ref(n: u64) -> FollowEvent<BlockRef<H256>> {
        FollowEvent::Initialized(Initialized {
            finalized_block_hash: BlockRef::new(H256::from_low_u64_le(n)),
            finalized_block_runtime: None,
        })
    }

    /// A new block event containing a BlockRef (useful for comparisons)
    pub fn ev_new_block_ref(parent: u64, n: u64) -> FollowEvent<BlockRef<H256>> {
        FollowEvent::NewBlock(NewBlock {
            parent_block_hash: BlockRef::new(H256::from_low_u64_le(parent)),
            block_hash: BlockRef::new(H256::from_low_u64_le(n)),
            new_runtime: None,
        })
    }

    /// A best block event containing a BlockRef (useful for comparisons)
    pub fn ev_best_block_ref(n: u64) -> FollowEvent<BlockRef<H256>> {
        FollowEvent::BestBlockChanged(BestBlockChanged {
            best_block_hash: BlockRef::new(H256::from_low_u64_le(n)),
        })
    }

    /// A finalized event containing a BlockRef (useful for comparisons)
    pub fn ev_finalized_ref(ns: impl IntoIterator<Item = u64>) -> FollowEvent<BlockRef<H256>> {
        FollowEvent::Finalized(Finalized {
            finalized_block_hashes: ns
                .into_iter()
                .map(|h| BlockRef::new(H256::from_low_u64_le(h)))
                .collect(),
            pruned_block_hashes: vec![],
        })
    }
}

#[cfg(test)]
mod test {
    use super::super::follow_stream::test_utils::{
        ev_best_block, ev_finalized, ev_initialized, ev_new_block,
    };
    use super::test_utils::{ev_new_block_ref, test_unpin_stream_getter};
    use super::*;
    use crate::config::substrate::H256;

    #[tokio::test]
    async fn hands_back_blocks() {
        let (follow_unpin, _) = test_unpin_stream_getter(
            || {
                [
                    Ok(ev_new_block(0, 1)),
                    Ok(ev_new_block(1, 2)),
                    Ok(ev_new_block(2, 3)),
                    Err(Error::Other("ended".to_owned())),
                ]
            },
            10,
        );

        let out: Vec<_> = follow_unpin
            .filter_map(|e| async move { e.ok() })
            .collect()
            .await;

        assert_eq!(
            out,
            vec![
                FollowStreamMsg::Ready("sub_id_0".into()),
                FollowStreamMsg::Event(ev_new_block_ref(0, 1)),
                FollowStreamMsg::Event(ev_new_block_ref(1, 2)),
                FollowStreamMsg::Event(ev_new_block_ref(2, 3)),
            ]
        );
    }

    #[tokio::test]
    async fn unpins_old_blocks() {
        let (mut follow_unpin, unpin_rx) = test_unpin_stream_getter(
            || {
                [
                    Ok(ev_initialized(0)),
                    Ok(ev_finalized([1])),
                    Ok(ev_finalized([2])),
                    Ok(ev_finalized([3])),
                    Ok(ev_finalized([4])),
                    Ok(ev_finalized([5])),
                    Err(Error::Other("ended".to_owned())),
                ]
            },
            3,
        );

        let _r = follow_unpin.next().await.unwrap().unwrap();
        let _i0 = follow_unpin.next().await.unwrap().unwrap();
        unpin_rx.try_recv().expect_err("nothing unpinned yet");
        let _f1 = follow_unpin.next().await.unwrap().unwrap();
        unpin_rx.try_recv().expect_err("nothing unpinned yet");
        let _f2 = follow_unpin.next().await.unwrap().unwrap();
        unpin_rx.try_recv().expect_err("nothing unpinned yet");
        let _f3 = follow_unpin.next().await.unwrap().unwrap();

        // Max age is 3, so after block 3 finalized, block 0 becomes too old and is unpinned.
        let (hash, _) = unpin_rx
            .try_recv()
            .expect("unpin call should have happened");
        assert_eq!(hash, H256::from_low_u64_le(0));

        let _f4 = follow_unpin.next().await.unwrap().unwrap();

        // Block 1 is now too old and is unpinned.
        let (hash, _) = unpin_rx
            .try_recv()
            .expect("unpin call should have happened");
        assert_eq!(hash, H256::from_low_u64_le(1));

        let _f5 = follow_unpin.next().await.unwrap().unwrap();

        // Block 2 is now too old and is unpinned.
        let (hash, _) = unpin_rx
            .try_recv()
            .expect("unpin call should have happened");
        assert_eq!(hash, H256::from_low_u64_le(2));
    }

    #[tokio::test]
    async fn unpins_dropped_blocks() {
        let (mut follow_unpin, unpin_rx) = test_unpin_stream_getter(
            || {
                [
                    Ok(ev_initialized(0)),
                    Ok(ev_finalized([1])),
                    Ok(ev_finalized([2])),
                    Err(Error::Other("ended".to_owned())),
                ]
            },
            10,
        );

        let _r = follow_unpin.next().await.unwrap().unwrap();
        let _i0 = follow_unpin.next().await.unwrap().unwrap();
        let f1 = follow_unpin.next().await.unwrap().unwrap();

        // We don't care about block 1 any more; drop it. unpins happen at finalized evs.
        drop(f1);

        let _f2 = follow_unpin.next().await.unwrap().unwrap();

        // Check that we get the expected unpin event.
        let (hash, _) = unpin_rx
            .try_recv()
            .expect("unpin call should have happened");
        assert_eq!(hash, H256::from_low_u64_le(1));

        // Confirm that 0 and 2 are still pinned and that 1 isnt.
        assert!(!follow_unpin.is_pinned(&H256::from_low_u64_le(1)));
        assert!(follow_unpin.is_pinned(&H256::from_low_u64_le(0)));
        assert!(follow_unpin.is_pinned(&H256::from_low_u64_le(2)));
    }

    #[tokio::test]
    async fn only_unpins_if_finalized_is_dropped() {
        // If we drop the "new block" and "best block" BlockRefs,
        // and then the block comes back as finalized (for instance),
        // no unpin call should be made unless we also drop the finalized
        // one.
        let (mut follow_unpin, unpin_rx) = test_unpin_stream_getter(
            || {
                [
                    Ok(ev_initialized(0)),
                    Ok(ev_new_block(0, 1)),
                    Ok(ev_best_block(1)),
                    Ok(ev_finalized([1])),
                    Ok(ev_finalized([2])),
                    Err(Error::Other("ended".to_owned())),
                ]
            },
            100,
        );

        let _r = follow_unpin.next().await.unwrap().unwrap();
        let _i0 = follow_unpin.next().await.unwrap().unwrap();
        let n1 = follow_unpin.next().await.unwrap().unwrap();
        drop(n1);
        let b1 = follow_unpin.next().await.unwrap().unwrap();
        drop(b1);
        let f1 = follow_unpin.next().await.unwrap().unwrap();

        // even though we dropped our block 1 in the new/best events, it won't be unpinned
        // because it occurred again in finalized event.
        unpin_rx.try_recv().expect_err("nothing unpinned yet");

        drop(f1);
        let _f2 = follow_unpin.next().await.unwrap().unwrap();

        // Since we dropped the finalized block 1, we'll now unpin it when next block finalized.
        let (hash, _) = unpin_rx.try_recv().expect("unpin should have happened now");
        assert_eq!(hash, H256::from_low_u64_le(1));
    }
}
