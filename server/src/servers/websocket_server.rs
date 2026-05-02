use std::{
    collections::{HashMap, HashSet},
    env::home_dir,
    sync::Arc,
};

use axum::{
    Router,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use log::{error, info};
use tokio::{
    net::TcpListener,
    select,
    sync::{
        Mutex,
        broadcast::{Sender, channel},
        mpsc,
    },
};
use yawc::{FrameView, OpCode, WebSocket};

use crate::{
    listeners::order_book::{
        InternalMessage, L2SnapshotParams, L2Snapshots, OrderBookListener, TimedSnapshots, hl_listen,
    },
    order_book::Coin,
    prelude::*,
    types::{
        L2Book, L4Book, L4BookUpdates, L4Order, Trade,
        node_data::{Batch, NodeDataFill, NodeDataOrderDiff, NodeDataOrderStatus},
        subscription::{ClientMessage, DEFAULT_LEVELS, ServerResponse, Subscription, SubscriptionManager},
    },
};

pub async fn run_websocket_server(
    address: &str,
    ignore_spot: bool,
    compression_level: u32,
    inactivity_exit_secs: u64,
) -> Result<()> {
    let (internal_message_tx, _) = channel::<Arc<InternalMessage>>(1024);

    let home_dir = home_dir().ok_or("Could not find home directory")?;
    let listener = {
        let internal_message_tx = internal_message_tx.clone();
        OrderBookListener::new(Some(internal_message_tx), ignore_spot)
    };
    let listener = Arc::new(Mutex::new(listener));
    {
        let listener = listener.clone();
        tokio::spawn(async move {
            if let Err(err) = hl_listen(listener, home_dir, inactivity_exit_secs).await {
                error!("Listener fatal error: {err}");
                std::process::exit(1);
            }
        });
    }

    let websocket_opts =
        yawc::Options::default().with_compression_level(yawc::CompressionLevel::new(compression_level));
    let app = Router::new().route(
        "/ws",
        get({
            let internal_message_tx = internal_message_tx.clone();
            async move |ws_upgrade| {
                ws_handler(ws_upgrade, internal_message_tx.clone(), listener.clone(), ignore_spot, websocket_opts)
            }
        }),
    );

    let listener = TcpListener::bind(address).await?;
    info!("WebSocket server running at ws://{address}");

    if let Err(err) = axum::serve(listener, app.into_make_service()).await {
        error!("Server fatal error: {err}");
        std::process::exit(2);
    }

    Ok(())
}

fn ws_handler(
    incoming: yawc::IncomingUpgrade,
    internal_message_tx: Sender<Arc<InternalMessage>>,
    listener: Arc<Mutex<OrderBookListener>>,
    ignore_spot: bool,
    websocket_opts: yawc::Options,
) -> Response {
    let (resp, fut) = match incoming.upgrade(websocket_opts) {
        Ok(ok) => ok,
        Err(err) => {
            error!("failed to start websocket upgrade: {err}");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    tokio::spawn(async move {
        let ws = match fut.await {
            Ok(ok) => ok,
            Err(err) => {
                log::error!("failed to upgrade websocket connection: {err}");
                return;
            }
        };

        handle_socket(ws, internal_message_tx, listener, ignore_spot).await;
    });

    resp.into_response()
}

async fn handle_socket(
    socket: WebSocket,
    internal_message_tx: Sender<Arc<InternalMessage>>,
    listener: Arc<Mutex<OrderBookListener>>,
    ignore_spot: bool,
) {
    let (mut socket_write, mut socket_read) = socket.split();
    let mut internal_message_rx = internal_message_tx.subscribe();

    let is_ready = listener.lock().await.is_ready();
    let mut manager = SubscriptionManager::default();
    let universe_set = listener.lock().await.universe();
    let mut universe: HashSet<String> = universe_set.into_iter().map(|c| c.value()).collect();

    if !is_ready {
        let msg = ServerResponse::Error("Order book not ready for streaming (waiting for snapshot)".to_string());
        if let Ok(payload) = sonic_rs::to_string(&msg) {
            drop(socket_write.send(FrameView::text(payload)).await);
        }
        return;
    }

    let (out_tx, mut out_rx) = mpsc::channel::<ServerResponse>(1024);

    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Ok(payload) = sonic_rs::to_string(&msg) {
                if let Err(e) = socket_write.send(FrameView::text(payload)).await {
                    error!("Write task error: {e}");
                    break;
                }
            }
        }
    });

    loop {
        select! {
            recv_result = internal_message_rx.recv() => {
                match recv_result {
                    Ok(msg) => {
                        match msg.as_ref() {
                            InternalMessage::Snapshot{ l2_snapshots, time } => {
                                universe = new_universe(l2_snapshots, ignore_spot);
                                for sub in manager.subscriptions() {
                                    if let Some(resp) = prepare_l2_snapshot_response(sub, l2_snapshots.as_ref(), *time) {
                                        drop(out_tx.try_send(resp));
                                    }
                                }
                            },
                            InternalMessage::Fills { trades } => {
                                for sub in manager.subscriptions() {
                                    if let Subscription::Trades { coin } = sub {
                                        if let Some(t) = trades.get(coin) {
                                            drop(out_tx.try_send(ServerResponse::Trades(t.clone())));
                                        }
                                    }
                                }
                            },
                            InternalMessage::L4BookUpdates { updates } => {
                                for sub in manager.subscriptions() {
                                    if let Subscription::L4Book { coin } = sub {
                                        if let Some(upd) = updates.get(coin) {
                                            drop(out_tx.try_send(ServerResponse::L4Book(L4Book::Updates(upd.clone()))));
                                        }
                                    }
                                }
                            },
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
            msg = socket_read.next() => {
                if let Some(frame) = msg {
                    if frame.opcode == OpCode::Text {
                        if let Ok(text) = std::str::from_utf8(&frame.payload) {
                            if let Ok(value) = sonic_rs::from_str::<ClientMessage>(text) {
                                receive_client_message(&out_tx, &mut manager, value, &universe, listener.clone()).await;
                            }
                        }
                    } else if frame.opcode == OpCode::Close { return; }
                } else { return; }
            }
        }
    }
}

async fn receive_client_message(
    out_tx: &mpsc::Sender<ServerResponse>,
    manager: &mut SubscriptionManager,
    client_message: ClientMessage,
    universe: &HashSet<String>,
    listener: Arc<Mutex<OrderBookListener>>,
) {
    let subscription = match &client_message {
        ClientMessage::Unsubscribe { subscription } | ClientMessage::Subscribe { subscription } => subscription.clone(),
    };

    if !subscription.validate(universe) {
        drop(out_tx.try_send(ServerResponse::Error("Invalid subscription".to_string())));
        return;
    }

    let success = match &client_message {
        ClientMessage::Subscribe { .. } => manager.subscribe(subscription.clone()),
        ClientMessage::Unsubscribe { .. } => manager.unsubscribe(subscription.clone()),
    };

    if success {
        if let ClientMessage::Subscribe { subscription } = &client_message {
            if let Ok(Some(snap)) = subscription.handle_immediate_snapshot(listener).await {
                drop(out_tx.try_send(snap));
            }
        }
        drop(out_tx.try_send(ServerResponse::SubscriptionResponse(client_message)));
    }
}

fn prepare_l2_snapshot_response(
    subscription: &Subscription,
    l2_snapshots: &L2Snapshots,
    time: u64,
) -> Option<ServerResponse> {
    if let Subscription::L2Book { coin, n_sig_figs, n_levels, mantissa } = subscription {
        let coin_obj = Coin::new(coin);
        let params = L2SnapshotParams::new(*n_sig_figs, *mantissa);

        let l2_snap = l2_snapshots.as_ref().get(&coin_obj)?.get(&params)?;
        let n = n_levels.unwrap_or(DEFAULT_LEVELS);
        let truncated = l2_snap.truncate(n);
        let exported = truncated.export_inner_snapshot();

        return Some(ServerResponse::L2Book(L2Book::from_l2_snapshot(coin.clone(), exported, time)));
    }
    None
}

fn new_universe(l2_snapshots: &L2Snapshots, ignore_spot: bool) -> HashSet<String> {
    l2_snapshots
        .as_ref()
        .iter()
        .filter_map(|(c, _)| if !c.is_spot() || !ignore_spot { Some(c.clone().value()) } else { None })
        .collect()
}

pub(crate) fn coin_to_trades(batch: &Batch<NodeDataFill>) -> HashMap<String, Vec<Trade>> {
    let fills = batch.clone().events();
    let mut trades: HashMap<String, Vec<Trade>> = HashMap::new();
    for fill in fills {
        let trade = Trade::from_single_fill(fill);
        let coin = trade.coin.clone();
        trades.entry(coin).or_insert_with(Vec::new).push(trade);
    }
    trades
}

pub(crate) fn coin_to_book_updates(
    diff_batch: &Batch<NodeDataOrderDiff>,
    status_batch: &Batch<NodeDataOrderStatus>,
) -> HashMap<String, L4BookUpdates> {
    let time = diff_batch.block_time();
    let height = diff_batch.block_number();
    let mut updates = HashMap::new();
    for diff in diff_batch.clone().events() {
        let coin = diff.coin().value();
        updates.entry(coin).or_insert_with(|| L4BookUpdates::new(time, height)).book_diffs.push(diff);
    }
    for status in status_batch.clone().events() {
        let coin = status.order.coin.clone();
        updates.entry(coin).or_insert_with(|| L4BookUpdates::new(time, height)).order_statuses.push(status);
    }
    updates
}

impl Subscription {
    async fn handle_immediate_snapshot(
        &self,
        listener: Arc<Mutex<OrderBookListener>>,
    ) -> Result<Option<ServerResponse>> {
        if let Self::L4Book { coin } = self {
            let state = listener.lock().await.compute_snapshot();
            if let Some(TimedSnapshots { time, height, snapshot }) = state {
                let coin_key = Coin::new(coin);
                if let Some((c, s)) = snapshot.value().into_iter().find(|(c, _)| *c == coin_key) {
                    let levels = s.as_ref().clone().map(|orders| orders.into_iter().map(L4Order::from).collect());
                    return Ok(Some(ServerResponse::L4Book(L4Book::Snapshot {
                        coin: c.value(),
                        time,
                        height,
                        levels,
                    })));
                }
            }
            return Err("Snapshot Failed".into());
        }
        Ok(None)
    }
}
