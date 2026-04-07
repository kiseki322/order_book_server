use crate::{
    listeners::order_book::{L2Snapshots, TimedSnapshots, utils::compute_l2_snapshots},
    order_book::{
        Coin, InnerOrder, Oid,
        multi_book::{OrderBooks, Snapshots},
    },
    prelude::*,
    types::{
        inner::{InnerL4Order, InnerOrderDiff},
        node_data::{Batch, EventSource, NodeDataOrderDiff, NodeDataOrderStatus},
    },
};
use std::collections::{HashMap, HashSet};

#[derive(Clone)]
pub(super) struct OrderBookState {
    order_book: OrderBooks<InnerL4Order>,
    height: u64,
    time: u64,
    ignore_spot: bool,
    pending_order_statuses: HashMap<Oid, NodeDataOrderStatus>,
    pending_new_diffs: HashMap<Oid, crate::order_book::types::Sz>,
}

impl OrderBookState {
    pub(super) fn from_snapshot(
        snapshot: Snapshots<InnerL4Order>,
        height: u64,
        time: u64,
        ignore_triggers: bool,
        ignore_spot: bool,
    ) -> Self {
        Self {
            ignore_spot,
            time,
            height,
            order_book: OrderBooks::from_snapshots(snapshot, ignore_triggers),
            pending_order_statuses: HashMap::with_capacity(10000),
            pending_new_diffs: HashMap::with_capacity(1000),
        }
    }

    pub(super) const fn height(&self) -> u64 {
        self.height
    }

    pub(super) const fn time(&self) -> u64 {
        self.time
    }

    pub(super) fn compute_snapshot(&self) -> TimedSnapshots {
        TimedSnapshots { time: self.time, height: self.height, snapshot: self.order_book.to_snapshots_par() }
    }

    pub(super) fn l2_snapshots_uncached(&self) -> (u64, L2Snapshots) {
        (self.time, compute_l2_snapshots(&self.order_book))
    }

    pub(super) fn compute_universe(&self) -> HashSet<Coin> {
        self.order_book.as_ref().keys().cloned().collect()
    }

    pub(super) fn pending_order_statuses_count(&self) -> usize {
        self.pending_order_statuses.len()
    }

    pub(super) fn pending_new_diffs_count(&self) -> usize {
        self.pending_new_diffs.len()
    }

    pub(super) fn order_count(&self) -> usize {
        self.order_book.order_count()
    }

    pub(super) fn coin_count(&self) -> usize {
        self.order_book.as_ref().len()
    }

    pub(super) fn cleanup_stale_pending(&mut self) {
        const MAX_PENDING_ORDERS: usize = 10_000;
        const MAX_PENDING_DIFFS: usize = 1_000;

        if self.pending_order_statuses.len() > MAX_PENDING_ORDERS {
            self.pending_order_statuses.clear();
        }

        if self.pending_new_diffs.len() > MAX_PENDING_DIFFS {
            self.pending_new_diffs.clear();
        }
    }

    pub(super) fn get_bbos_for_coins(
        &self,
        coins: &HashSet<Coin>,
    ) -> (
        u64,
        HashMap<
            Coin,
            (
                Option<(crate::order_book::Px, crate::order_book::Sz, u32)>,
                Option<(crate::order_book::Px, crate::order_book::Sz, u32)>,
            ),
        >,
    ) {
        (self.time, self.order_book.get_bbos_for_coins(coins))
    }

    pub(super) fn apply_order_statuses_hft(&mut self, batch: Batch<NodeDataOrderStatus>) -> Result<HashSet<Coin>> {
        let height = batch.block_number();
        if height >= self.height {
            self.height = height;
            self.time = batch.block_time();
        }

        let mut changed_coins = HashSet::with_capacity(8);
        for order_status in batch.into_events() {
            let oid = Oid::new(order_status.order.oid);

            if let Some(sz) = self.pending_new_diffs.remove(&oid) {
                let order_coin = Coin::new(&order_status.order.coin);
                let timestamp = order_status.time.and_utc().timestamp_millis();
                let mut inner_order: InnerL4Order = order_status.try_into()?;

                inner_order.modify_sz(sz);
                inner_order.convert_trigger(timestamp as u64);

                self.order_book.add_order(inner_order);
                changed_coins.insert(order_coin);
            } else if order_status.is_inserted_into_book() {
                self.pending_order_statuses.insert(oid, order_status);
            }
        }
        Ok(changed_coins)
    }

    pub(super) fn apply_order_diffs_hft(&mut self, batch: Batch<NodeDataOrderDiff>) -> Result<HashSet<Coin>> {
        let height = batch.block_number();
        if height >= self.height {
            self.height = height;
            self.time = batch.block_time();
        }

        let mut changed_coins = HashSet::with_capacity(8);
        for diff in batch.into_events() {
            let coin = diff.coin();
            if coin.is_spot() && self.ignore_spot {
                continue;
            }

            let oid = diff.oid();
            let inner_diff = diff.diff().try_into()?;

            match inner_diff {
                InnerOrderDiff::New { sz } => {
                    if let Some(order_status) = self.pending_order_statuses.remove(&oid) {
                        let order_coin = Coin::new(&order_status.order.coin);
                        let timestamp = order_status.time.and_utc().timestamp_millis();
                        let mut inner_order: InnerL4Order = order_status.try_into()?;

                        inner_order.modify_sz(sz);
                        inner_order.convert_trigger(timestamp as u64);

                        self.order_book.add_order(inner_order);
                        changed_coins.insert(order_coin);
                    } else {
                        self.pending_new_diffs.insert(oid, sz);
                    }
                }
                InnerOrderDiff::Update { new_sz, .. } => {
                    let _ = self.order_book.modify_sz(oid, coin.clone(), new_sz);
                    changed_coins.insert(coin);
                }
                InnerOrderDiff::Remove => {
                    let _ = self.order_book.cancel_order(oid, coin.clone());
                    changed_coins.insert(coin);
                }
            }
        }
        Ok(changed_coins)
    }
}
