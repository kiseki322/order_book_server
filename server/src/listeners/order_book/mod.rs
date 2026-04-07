use crate::{
    listeners::order_book::state::OrderBookState,
    order_book::{
        Coin, Px, Snapshot, Sz,
        multi_book::{Snapshots, load_snapshots_from_cli_json},
    },
    prelude::*,
    types::{
        L4Order,
        inner::{InnerL4Order, InnerLevel},
        node_data::{Batch, EventSource, NodeDataFill, NodeDataOrderDiff, NodeDataOrderStatus},
    },
};
use alloy::primitives::Address;
use log::{error, info};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{
        Mutex,
        broadcast::Sender,
        mpsc::{UnboundedSender, unbounded_channel},
    },
    time::{Instant, sleep},
};
use utils::{EventBatch, SnapshotConfig, get_visor_path, process_rmp_file};

mod parallel;
mod state;
mod utils;

fn fetch_snapshot(
    snapshot_config: SnapshotConfig,
    listener: Arc<Mutex<OrderBookListener>>,
    tx: UnboundedSender<Result<()>>,
    _ignore_spot: bool,
) {
    tokio::spawn(async move {
        {
            let mut listener = listener.lock().await;
            listener.begin_caching();
        }

        let visor_path = get_visor_path(&snapshot_config);
        let res = match process_rmp_file(&snapshot_config).await {
            Ok(output_fln) => {
                let snapshot =
                    load_snapshots_from_cli_json::<InnerL4Order, (Address, L4Order)>(&output_fln, &visor_path).await;
                info!("Snapshot fetched");
                sleep(Duration::from_secs(1)).await;

                match snapshot {
                    Ok((height, expected_snapshot)) => {
                        info!("Snapshot loaded at height {}", height);
                        let mut listener = listener.lock().await;
                        let _cache = listener.take_cache();
                        listener.init_from_snapshot(expected_snapshot, height);
                        Ok(())
                    }
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(err),
        };
        let _ = tx.send(res);
    });
}

pub(crate) struct OrderBookListener {
    ignore_spot: bool,
    order_book_state: Option<OrderBookState>,
    fetched_snapshot_cache: Option<VecDeque<(Batch<NodeDataOrderStatus>, Batch<NodeDataOrderDiff>)>>,
    internal_message_tx: Option<Sender<Arc<InternalMessage>>>,
    last_l2_broadcast: Option<Instant>,
}

impl OrderBookListener {
    pub(crate) const fn new(internal_message_tx: Option<Sender<Arc<InternalMessage>>>, ignore_spot: bool) -> Self {
        Self {
            ignore_spot,
            order_book_state: None,
            fetched_snapshot_cache: None,
            internal_message_tx,
            last_l2_broadcast: None,
        }
    }

    pub(crate) const fn is_ready(&self) -> bool {
        self.order_book_state.is_some()
    }

    pub(crate) fn universe(&self) -> HashSet<Coin> {
        self.order_book_state.as_ref().map_or_else(HashSet::new, OrderBookState::compute_universe)
    }

    fn begin_caching(&mut self) {
        self.fetched_snapshot_cache = Some(VecDeque::new());
    }

    fn take_cache(&mut self) -> VecDeque<(Batch<NodeDataOrderStatus>, Batch<NodeDataOrderDiff>)> {
        self.fetched_snapshot_cache.take().unwrap_or_default()
    }

    fn init_from_snapshot(&mut self, snapshot: Snapshots<InnerL4Order>, height: u64) {
        let new_order_book = OrderBookState::from_snapshot(snapshot, height, 0, true, self.ignore_spot);
        self.order_book_state = Some(new_order_book);
        self.fetched_snapshot_cache = None;
        info!("Order book ready at height {}", height);
    }

    pub(crate) fn compute_snapshot(&mut self) -> Option<TimedSnapshots> {
        self.order_book_state.as_mut().map(|o| o.compute_snapshot())
    }

    #[inline(always)]
    fn should_broadcast_l2(&self) -> bool {
        self.last_l2_broadcast.map(|t| t.elapsed() >= Duration::from_millis(10)).unwrap_or(true)
    }

    pub(crate) fn process_data_hft(&mut self, line: String, event_source: EventSource) -> Result<()> {
        if line.is_empty() {
            return Ok(());
        }

        let event_batch = match event_source {
            EventSource::Fills => {
                let batch = sonic_rs::from_str::<Batch<NodeDataFill>>(&line)?;
                EventBatch::Fills(batch)
            }
            EventSource::OrderStatuses => {
                let batch = sonic_rs::from_str::<Batch<NodeDataOrderStatus>>(&line)?;
                EventBatch::Orders(batch)
            }
            EventSource::OrderDiffs => {
                let batch = sonic_rs::from_str::<Batch<NodeDataOrderDiff>>(&line)?;
                EventBatch::BookDiffs(batch)
            }
        };

        let changed_coins = if let Some(state) = self.order_book_state.as_mut() {
            match event_batch {
                EventBatch::Orders(batch) => {
                    if let Some(tx) = &self.internal_message_tx {
                        let _ = tx.send(Arc::new(InternalMessage::L4OrderStatuses { batch: batch.clone() }));
                    }
                    state.apply_order_statuses_hft(batch)?
                }
                EventBatch::BookDiffs(batch) => {
                    if let Some(tx) = &self.internal_message_tx {
                        let _ = tx.send(Arc::new(InternalMessage::L4OrderDiffs { batch: batch.clone() }));
                    }
                    state.apply_order_diffs_hft(batch)?
                }
                EventBatch::Fills(batch) => {
                    if let Some(tx) = &self.internal_message_tx {
                        let _ = tx.send(Arc::new(InternalMessage::Fills { batch }));
                    }
                    HashSet::new()
                }
            }
        } else {
            HashSet::new()
        };

        if !changed_coins.is_empty() {
            if let Some(state) = &self.order_book_state {
                let (time, bbos) = state.get_bbos_for_coins(&changed_coins);
                if let Some(tx) = &self.internal_message_tx {
                    let _ = tx.send(Arc::new(InternalMessage::BboUpdate { bbos, time }));
                }
            }
        }

        if self.should_broadcast_l2() {
            if let Some(state) = &self.order_book_state {
                let (time, l2_snapshots) = state.l2_snapshots_uncached();
                if let Some(tx) = &self.internal_message_tx {
                    self.last_l2_broadcast = Some(Instant::now());
                    let _ = tx.send(Arc::new(InternalMessage::Snapshot { l2_snapshots, time }));
                }
            }
        }

        Ok(())
    }
}

pub(crate) struct L2Snapshots(HashMap<Coin, HashMap<L2SnapshotParams, Snapshot<InnerLevel>>>);

impl L2Snapshots {
    pub(crate) const fn as_ref(&self) -> &HashMap<Coin, HashMap<L2SnapshotParams, Snapshot<InnerLevel>>> {
        &self.0
    }
}

pub(crate) struct TimedSnapshots {
    pub(crate) time: u64,
    pub(crate) height: u64,
    pub(crate) snapshot: Snapshots<InnerL4Order>,
}

pub(crate) enum InternalMessage {
    Snapshot { l2_snapshots: L2Snapshots, time: u64 },
    Fills { batch: Batch<NodeDataFill> },
    BboUpdate { bbos: HashMap<Coin, (Option<(Px, Sz, u32)>, Option<(Px, Sz, u32)>)>, time: u64 },
    L4OrderDiffs { batch: Batch<NodeDataOrderDiff> },
    L4OrderStatuses { batch: Batch<NodeDataOrderStatus> },
}

#[derive(Eq, PartialEq, Hash)]
pub(crate) struct L2SnapshotParams {
    n_sig_figs: Option<u32>,
    mantissa: Option<u64>,
}

pub(crate) async fn hl_listen_hft(listener: Arc<Mutex<OrderBookListener>>, config: crate::ServerConfig) -> Result<()> {
    let dir = config.data_dir.clone().unwrap_or_else(|| dirs::home_dir().expect("Could not find home directory"));

    let snapshot_config = SnapshotConfig {
        mode: config.snapshot_mode,
        docker_container: config.docker_container.clone(),
        hlnode_binary: config.hlnode_binary.clone(),
        abci_state_path: config.abci_state_path.clone(),
        snapshot_output_path: config.snapshot_output_path.clone(),
        visor_state_path: config.visor_state_path.clone(),
        data_dir: dir.clone(),
    };

    let ignore_spot = listener.lock().await.ignore_spot;
    let (crossbeam_rx, _handles, _, _, _) = parallel::start_parallel_file_watchers(dir);
    let (tokio_tx, mut tokio_rx) = unbounded_channel::<parallel::FileEvent>();

    tokio::task::spawn_blocking(move || {
        while let Ok(event) = crossbeam_rx.recv() {
            if tokio_tx.send(event).is_err() {
                break;
            }
        }
    });

    let (snapshot_fetch_task_tx, mut snapshot_fetch_task_rx) = unbounded_channel::<Result<()>>();
    let mut ticker = tokio::time::interval_at(Instant::now() + Duration::from_secs(5), Duration::from_secs(10));
    let mut snapshot_fetch_pending = false;

    loop {
        tokio::select! {
            biased;
            Some(event) = tokio_rx.recv() => {
                let mut lock = listener.lock().await;
                let result = match event {
                    parallel::FileEvent::OrderDiff(line) => lock.process_data_hft(line, EventSource::OrderDiffs),
                    parallel::FileEvent::OrderStatus(line) => lock.process_data_hft(line, EventSource::OrderStatuses),
                    parallel::FileEvent::Fill(line) => lock.process_data_hft(line, EventSource::Fills),
                };
                if let Err(err) = result {
                    error!("HFT process error: {err}");
                }
            }
            res = snapshot_fetch_task_rx.recv() => {
                snapshot_fetch_pending = false;
                if let Some(Err(err)) = res { return Err(err); }
            }
            _ = ticker.tick() => {
                let is_ready = listener.lock().await.is_ready();
                if !is_ready && !snapshot_fetch_pending {
                    snapshot_fetch_pending = true;
                    fetch_snapshot(snapshot_config.clone(), listener.clone(), snapshot_fetch_task_tx.clone(), ignore_spot);
                }
            }
        }
    }
}
