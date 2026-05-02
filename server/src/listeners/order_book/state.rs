use std::collections::{HashMap, HashSet, VecDeque};

use crate::{
    listeners::order_book::{L2Snapshots, TimedSnapshots, utils::compute_l2_snapshots},
    order_book::{
        Coin, InnerOrder, Oid, Px,
        multi_book::{OrderBooks, Snapshots},
    },
    prelude::*,
    types::{
        inner::{InnerL4Order, InnerOrderDiff},
        node_data::{Batch, NodeDataOrderDiff, NodeDataOrderStatus},
    },
};

#[derive(Clone)]
pub(super) struct OrderBookState {
    order_book: OrderBooks<InnerL4Order>,
    height: u64,
    time: u64,
    snapped: bool,
    ignore_spot: bool,
    cached_universe: Option<HashSet<Coin>>,
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
            snapped: false,
            cached_universe: None,
        }
    }

    pub(super) const fn height(&self) -> u64 {
        self.height
    }

    // forcibly take snapshot - (time, height, snapshot)
    pub(super) fn compute_snapshot(&self) -> TimedSnapshots {
        TimedSnapshots { time: self.time, height: self.height, snapshot: self.order_book.to_snapshots_par() }
    }

    // (time, snapshot)
    pub(super) fn l2_snapshots(&mut self, prevent_future_snaps: bool) -> Option<(u64, L2Snapshots)> {
        if self.snapped {
            None
        } else {
            self.snapped = prevent_future_snaps || self.snapped;
            Some((self.time, compute_l2_snapshots(&self.order_book)))
        }
    }

    pub(super) fn compute_universe(&mut self) -> HashSet<Coin> {
        if let Some(ref universe) = self.cached_universe {
            universe.clone()
        } else {
            let universe: HashSet<Coin> = self.order_book.as_ref().keys().cloned().collect();
            self.cached_universe = Some(universe.clone());
            universe
        }
    }

    pub(super) fn apply_updates(
        &mut self,
        order_statuses: Batch<NodeDataOrderStatus>,
        order_diffs: Batch<NodeDataOrderDiff>,
    ) -> Result<()> {
        let height = order_statuses.block_number();
        let time = order_statuses.block_time();
        assert_eq!(order_statuses.block_number(), order_diffs.block_number());
        if height > self.height + 1 {
            return Err(format!("Expecting block {}, got block {}", self.height + 1, height).into());
        } else if height <= self.height {
            // This is not an error in case we started caching long before a snapshot is fetched
            return Ok(());
        }
        let mut diffs = order_diffs.events().into_iter().collect::<VecDeque<_>>();
        let mut order_map = order_statuses
            .events()
            .into_iter()
            .filter_map(|order_status| {
                if order_status.is_inserted_into_book() {
                    Some((Oid::new(order_status.order.oid), order_status))
                } else {
                    None
                }
            })
            .collect::<HashMap<_, _>>();
        while let Some(diff) = diffs.pop_front() {
            let oid = diff.oid();
            let coin = diff.coin();
            if coin.is_spot() && self.ignore_spot {
                continue;
            }
            let inner_diff = diff.diff().try_into()?;
            match inner_diff {
                InnerOrderDiff::New { sz } => {
                    if let Some(order) = order_map.remove(&oid) {
                        let time = order.time.and_utc().timestamp_millis();
                        let mut inner_order: InnerL4Order = order.try_into()?;
                        inner_order.modify_sz(sz);
                        #[allow(clippy::unwrap_used)]
                        inner_order.convert_trigger(time.try_into().unwrap());
                        self.order_book.add_order(inner_order);
                        self.cached_universe = None;
                    } else if diff.special_address() {
                        // Assume all orders from special addresses are Alo, Limit orders
                        let inner_order = InnerL4Order {
                            user: diff.user(),
                            coin,
                            side: diff.side(),
                            limit_px: Px::parse_from_str(diff.px().as_str())?,
                            sz,
                            oid: oid.value(),
                            timestamp: time,
                            trigger_condition: "N/A".to_string(),
                            is_trigger: false,
                            trigger_px: "0.0".to_string(),
                            is_position_tpsl: false,
                            reduce_only: false,
                            order_type: "Limit".to_string(),
                            tif: Some("Alo".to_string()),
                            cloid: None,
                        };
                        self.order_book.add_order(inner_order);
                        self.cached_universe = None;
                    } else {
                        log::warn!("Unable to find order opening status for oid: {oid:?}. Skipping.");
                        continue;
                    }
                }
                InnerOrderDiff::Update { new_sz, .. } => {
                    if !self.order_book.modify_sz(oid.clone(), coin, new_sz) {
                        log::warn!("Unable to find order on the book for Update. oid: {oid:?}. Skipping.");
                        continue;
                    }
                }
                InnerOrderDiff::Remove => {
                    if !self.order_book.cancel_order(oid.clone(), coin) {
                        log::warn!("Unable to find order on the book for Remove. oid: {oid:?}. Skipping.");
                        continue;
                    }
                }
            }
        }
        self.height += 1;
        self.time = time;
        self.snapped = false;
        Ok(())
    }
}
