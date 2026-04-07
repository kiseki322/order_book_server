use crate::ServerConfig;
use crate::{
    listeners::order_book::{
        InternalMessage, L2SnapshotParams, L2Snapshots, OrderBookListener, TimedSnapshots, hl_listen_hft,
    },
    order_book::{Coin, Px, Snapshot, Sz},
    prelude::*,
    types::{
        Bbo, L2Book, L4Book, L4BookUpdates, L4Order, Trade,
        inner::InnerLevel,
        node_data::{Batch, NodeDataFill, NodeDataOrderDiff, NodeDataOrderStatus},
        subscription::{ClientMessage, DEFAULT_LEVELS, OrderUpdate, ServerResponse, Subscription, SubscriptionManager},
    },
};
use axum::{Router, response::IntoResponse, routing::get};
use futures_util::{SinkExt, StreamExt};
use log::error;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::select;
use tokio::{
    net::TcpListener,
    sync::{
        Mutex,
        broadcast::{Sender, channel},
    },
};
use yawc::{FrameView, OpCode, WebSocket};

pub async fn run_websocket_server(config: ServerConfig) -> Result<()> {
    let (internal_message_tx, _) = channel::<Arc<InternalMessage>>(4096);

    let market_filter = (config.include_perps, config.include_spot, config.include_hip3);
    let ignore_spot = !config.include_spot;
    let compression_level = config.compression_level;

    let listener = {
        let internal_message_tx = internal_message_tx.clone();
        OrderBookListener::new(Some(internal_message_tx), ignore_spot)
    };
    let listener = Arc::new(Mutex::new(listener));
    {
        let listener = listener.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let result = hl_listen_hft(listener, config).await;
            if let Err(err) = result {
                error!("Listener fatal error: {err}");
                std::process::exit(1);
            }
        });
    }

    let websocket_opts =
        yawc::Options::default().with_compression_level(yawc::CompressionLevel::new(compression_level));

    let start_time = std::time::Instant::now();
    let listener_for_health = listener.clone();

    let app: Router = Router::new()
        .route(
            "/ws",
            get({
                let internal_message_tx = internal_message_tx.clone();
                let bbo_only = config.bbo_only;
                let listener = listener.clone();
                move |ws_upgrade| async move {
                    ws_handler(
                        ws_upgrade,
                        internal_message_tx.clone(),
                        listener.clone(),
                        market_filter,
                        bbo_only,
                        websocket_opts,
                    )
                }
            }),
        )
        .route(
            "/health",
            get(move || {
                let listener = listener_for_health.clone();
                async move {
                    let is_ready = listener.lock().await.is_ready();
                    let uptime_secs = start_time.elapsed().as_secs();
                    let body = format!(
                        r#"{{"status":"{}","uptime_seconds":{}}}"#,
                        if is_ready { "ready" } else { "initializing" },
                        uptime_secs,
                    );
                    axum::response::Response::builder().header("content-type", "application/json").body(body).unwrap()
                }
            }),
        );

    let tcp_listener = TcpListener::bind(&config.address).await?;
    if let Err(err) = axum::serve(tcp_listener, app).await {
        error!("Server fatal error: {err}");
        std::process::exit(2);
    }

    Ok(())
}

fn ws_handler(
    incoming: yawc::IncomingUpgrade,
    internal_message_tx: Sender<Arc<InternalMessage>>,
    listener: Arc<Mutex<OrderBookListener>>,
    market_filter: (bool, bool, bool),
    bbo_only: bool,
    websocket_opts: yawc::Options,
) -> impl IntoResponse {
    let (resp, fut) = incoming.upgrade(websocket_opts).unwrap();
    tokio::spawn(async move {
        let ws = match fut.await {
            Ok(ok) => ok,
            Err(_) => return,
        };
        handle_socket(ws, internal_message_tx, listener, market_filter, bbo_only).await
    });
    resp
}

async fn handle_socket(
    mut socket: WebSocket,
    internal_message_tx: Sender<Arc<InternalMessage>>,
    listener: Arc<Mutex<OrderBookListener>>,
    market_filter: (bool, bool, bool),
    bbo_only: bool,
) {
    let mut internal_message_rx = internal_message_tx.subscribe();
    let mut manager = SubscriptionManager::default();
    let mut universe: HashSet<String> = listener.lock().await.universe().into_iter().map(|c| c.value()).collect();
    let mut last_bbo: HashMap<String, (String, String, String, String)> = HashMap::new();
    let mut last_l2_hash: HashMap<String, u64> = HashMap::new();

    loop {
        select! {
            recv_result = internal_message_rx.recv() => {
                match recv_result {
                    Ok(msg) => {
                        match msg.as_ref() {
                            InternalMessage::Snapshot{ l2_snapshots, time } => {
                                universe = new_universe(l2_snapshots, market_filter.0, market_filter.1, market_filter.2);
                                for sub in manager.subscriptions() {
                                    if !matches!(sub, Subscription::Bbo { .. }) {
                                        send_ws_data_from_snapshot(&mut socket, sub, l2_snapshots.as_ref(), *time, &mut last_bbo, &mut last_l2_hash).await;
                                    }
                                }
                            },
                            InternalMessage::BboUpdate{ bbos, time } => {
                                for sub in manager.subscriptions() {
                                    if let Subscription::Bbo { coin } = sub {
                                        send_ws_data_from_bbo(&mut socket, coin, bbos, *time, &mut last_bbo).await;
                                    }
                                }
                            },
                            InternalMessage::Fills{ batch } => {
                                let has_trades = manager.subscriptions().iter().any(|s| matches!(s, Subscription::Trades { .. }));
                                if has_trades {
                                    let trades = coin_to_trades(batch);
                                    for sub in manager.subscriptions() {
                                        send_ws_data_from_trades(&mut socket, sub, &trades).await;
                                    }
                                }
                            },
                            InternalMessage::L4OrderDiffs{ batch } => {
                                let has_l4 = manager.subscriptions().iter().any(|s| matches!(s, Subscription::L4Book { .. }));
                                let has_book_diffs = manager.subscriptions().iter().any(|s| matches!(s, Subscription::BookDiffs { .. }));
                                if has_l4 || has_book_diffs {
                                    let book_updates = if has_l4 { Some(coin_to_book_diffs_only(batch)) } else { None };
                                    let raw_diffs = if has_book_diffs { Some(coin_to_book_diffs_raw(batch)) } else { None };
                                    for sub in manager.subscriptions() {
                                        if let (Some(updates), Subscription::L4Book { .. }) = (&book_updates, sub) {
                                            send_ws_data_from_book_updates(&mut socket, sub, updates).await;
                                        }
                                        if let (Some(diffs), Subscription::BookDiffs { .. }) = (&raw_diffs, sub) {
                                            send_ws_data_from_book_diffs_raw(&mut socket, sub, diffs).await;
                                        }
                                    }
                                }
                            },
                            InternalMessage::L4OrderStatuses{ batch } => {
                                let has_l4 = manager.subscriptions().iter().any(|s| matches!(s, Subscription::L4Book { .. }));
                                let has_order_updates = manager.subscriptions().iter().any(|s| matches!(s, Subscription::OrderUpdates { .. }));
                                if has_l4 {
                                    let book_updates = coin_to_book_statuses_only(batch);
                                    for sub in manager.subscriptions() {
                                        send_ws_data_from_book_updates(&mut socket, sub, &book_updates).await;
                                    }
                                }
                                if has_order_updates {
                                    for sub in manager.subscriptions() {
                                        send_ws_order_updates(&mut socket, sub, batch).await;
                                    }
                                }
                            },
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
            msg = socket.next() => {
                if let Some(frame) = msg {
                    match frame.opcode {
                        OpCode::Text => {
                            if let Ok(text) = std::str::from_utf8(&frame.payload) {
                                if let Ok(value) = serde_json::from_str::<ClientMessage>(text) {
                                    match value {
                                        ClientMessage::Ping => {
                                            send_socket_message(&mut socket, ServerResponse::Pong).await;
                                        }
                                        _ => {
                                            receive_client_message(&mut socket, &mut manager, value, &universe, listener.clone(), bbo_only).await;
                                        }
                                    }
                                }
                            }
                        }
                        OpCode::Close => return,
                        _ => {}
                    }
                } else {
                    return;
                }
            }
        }
    }
}

async fn receive_client_message(
    socket: &mut WebSocket,
    manager: &mut SubscriptionManager,
    client_message: ClientMessage,
    universe: &HashSet<String>,
    listener: Arc<Mutex<OrderBookListener>>,
    bbo_only: bool,
) {
    let subscription = match &client_message {
        ClientMessage::Unsubscribe { subscription } | ClientMessage::Subscribe { subscription } => subscription.clone(),
        _ => return,
    };
    if !subscription.validate(universe) {
        send_socket_message(socket, ServerResponse::Error("Invalid subscription".into())).await;
        return;
    }
    if bbo_only && !matches!(&subscription, Subscription::Bbo { .. }) {
        send_socket_message(socket, ServerResponse::Error("BBO-only mode active".into())).await;
        return;
    }
    let success = match &client_message {
        ClientMessage::Subscribe { .. } => manager.subscribe(subscription.clone()),
        ClientMessage::Unsubscribe { .. } => manager.unsubscribe(subscription.clone()),
        _ => false,
    };
    if success {
        let mut snapshot_msg = None;
        if let ClientMessage::Subscribe { subscription } = &client_message {
            if let Ok(Some(msg)) = subscription.handle_immediate_snapshot(listener).await {
                snapshot_msg = Some(msg);
            }
        }
        send_socket_message(socket, ServerResponse::SubscriptionResponse(client_message)).await;
        if let Some(msg) = snapshot_msg {
            send_socket_message(socket, msg).await;
        }
    }
}

async fn send_ws_data_from_bbo(
    socket: &mut WebSocket,
    coin: &str,
    bbos: &HashMap<Coin, (Option<(Px, Sz, u32)>, Option<(Px, Sz, u32)>)>,
    time: u64,
    last_bbo: &mut HashMap<String, (String, String, String, String)>,
) {
    if let Some((best_bid, best_ask)) = bbos.get(&Coin::new(coin)) {
        let bid = best_bid
            .as_ref()
            .map(|(p, s, n)| crate::types::Level::new(format!("{:?}", p), format!("{:?}", s), *n as usize));
        let ask = best_ask
            .as_ref()
            .map(|(p, s, n)| crate::types::Level::new(format!("{:?}", p), format!("{:?}", s), *n as usize));
        let current = (
            bid.as_ref().map(|b| b.px().to_string()).unwrap_or_default(),
            bid.as_ref().map(|b| b.sz().to_string()).unwrap_or_default(),
            ask.as_ref().map(|a| a.px().to_string()).unwrap_or_default(),
            ask.as_ref().map(|a| a.sz().to_string()).unwrap_or_default(),
        );
        if last_bbo.get(coin) != Some(&current) {
            last_bbo.insert(coin.to_string(), current);
            send_socket_message(socket, ServerResponse::Bbo(Bbo { coin: coin.to_string(), time, bid, ask })).await;
        }
    }
}

async fn send_socket_message(socket: &mut WebSocket, msg: ServerResponse) {
    if let Ok(payload) = serde_json::to_string(&msg) {
        let _ = socket.send(FrameView::text(payload)).await;
    }
}

fn new_universe(l2: &L2Snapshots, p: bool, s: bool, h: bool) -> HashSet<String> {
    l2.as_ref()
        .iter()
        .filter_map(|(c, _)| {
            if (c.is_perp() && p) || (c.is_spot() && s) || (c.is_hip3() && h) { Some(c.clone().value()) } else { None }
        })
        .collect()
}

async fn send_ws_data_from_snapshot(
    socket: &mut WebSocket,
    sub: &Subscription,
    snapshot: &HashMap<Coin, HashMap<L2SnapshotParams, Snapshot<InnerLevel>>>,
    time: u64,
    last_bbo: &mut HashMap<String, (String, String, String, String)>,
    last_l2_hash: &mut HashMap<String, u64>,
) {
    match sub {
        Subscription::L2Book { coin, n_sig_figs, n_levels, mantissa } => {
            if let Some(s) =
                snapshot.get(&Coin::new(coin)).and_then(|m| m.get(&L2SnapshotParams::new(*n_sig_figs, *mantissa)))
            {
                let n = n_levels.unwrap_or(DEFAULT_LEVELS);
                let export = s.truncate(n).export_inner_snapshot();
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                use std::hash::{Hash, Hasher};
                format!("{:?}", export).hash(&mut hasher);
                let h = hasher.finish();
                let key = format!("{}:{}:{}", coin, n_sig_figs.unwrap_or(0), mantissa.unwrap_or(0));
                if last_l2_hash.get(&key) != Some(&h) {
                    last_l2_hash.insert(key, h);
                    send_socket_message(
                        socket,
                        ServerResponse::L2Book(L2Book::from_l2_snapshot(
                            coin.clone(),
                            export,
                            time,
                            *n_sig_figs,
                            *mantissa,
                            Some(n),
                        )),
                    )
                    .await;
                }
            }
        }
        Subscription::Bbo { coin } => {
            if let Some(s) = snapshot.get(&Coin::new(coin)).and_then(|m| m.get(&L2SnapshotParams::new(None, None))) {
                let lvls = s.truncate(1).export_inner_snapshot();
                let b = lvls[0].first().cloned();
                let a = lvls[1].first().cloned();
                let cur = (
                    b.as_ref().map(|x| x.px().to_string()).unwrap_or_default(),
                    b.as_ref().map(|x| x.sz().to_string()).unwrap_or_default(),
                    a.as_ref().map(|x| x.px().to_string()).unwrap_or_default(),
                    a.as_ref().map(|x| x.sz().to_string()).unwrap_or_default(),
                );
                if last_bbo.get(coin) != Some(&cur) {
                    last_bbo.insert(coin.clone(), cur);
                    send_socket_message(socket, ServerResponse::Bbo(Bbo { coin: coin.clone(), time, bid: b, ask: a }))
                        .await;
                }
            }
        }
        _ => {}
    }
}

fn coin_to_trades(batch: &Batch<NodeDataFill>) -> HashMap<String, Vec<Trade>> {
    let mut trades = HashMap::new();
    for fill in batch.clone().events() {
        let trade = Trade::from_single_fill(fill);
        trades.entry(trade.coin.clone()).or_insert_with(Vec::new).push(trade);
    }
    trades
}

fn coin_to_book_diffs_only(batch: &Batch<NodeDataOrderDiff>) -> HashMap<String, L4BookUpdates> {
    let mut updates = HashMap::new();
    let time = batch.block_time();
    let height = batch.block_number();
    for diff in batch.clone().events() {
        let coin = diff.coin().value();
        updates.entry(coin).or_insert_with(|| L4BookUpdates::new(time, height)).book_diffs.push(diff);
    }
    updates
}

fn coin_to_book_statuses_only(batch: &Batch<NodeDataOrderStatus>) -> HashMap<String, L4BookUpdates> {
    let mut updates = HashMap::new();
    let time = batch.block_time();
    let height = batch.block_number();
    for status in batch.clone().events() {
        let coin = status.order.coin.clone();
        updates.entry(coin).or_insert_with(|| L4BookUpdates::new(time, height)).order_statuses.push(status);
    }
    updates
}

fn coin_to_book_diffs_raw(batch: &Batch<NodeDataOrderDiff>) -> HashMap<String, Vec<NodeDataOrderDiff>> {
    let mut grouped = HashMap::new();
    for diff in batch.clone().events() {
        grouped.entry(diff.coin().value()).or_insert_with(Vec::new).push(diff);
    }
    grouped
}

async fn send_ws_data_from_book_diffs_raw(
    socket: &mut WebSocket,
    sub: &Subscription,
    diffs: &HashMap<String, Vec<NodeDataOrderDiff>>,
) {
    if let Subscription::BookDiffs { coin } = sub {
        if let Some(d) = diffs.get(coin) {
            send_socket_message(socket, ServerResponse::BookDiffs(d.clone())).await;
        }
    }
}

async fn send_ws_data_from_book_updates(
    socket: &mut WebSocket,
    sub: &Subscription,
    updates: &HashMap<String, L4BookUpdates>,
) {
    if let Subscription::L4Book { coin } = sub {
        if let Some(u) = updates.get(coin) {
            send_socket_message(socket, ServerResponse::L4Book(L4Book::Updates(u.clone()))).await;
        }
    }
}

async fn send_ws_data_from_trades(socket: &mut WebSocket, sub: &Subscription, trades: &HashMap<String, Vec<Trade>>) {
    if let Subscription::Trades { coin } = sub {
        if let Some(t) = trades.get(coin) {
            send_socket_message(socket, ServerResponse::Trades(t.clone())).await;
        }
    }
}

async fn send_ws_order_updates(socket: &mut WebSocket, sub: &Subscription, batch: &Batch<NodeDataOrderStatus>) {
    if let Subscription::OrderUpdates { user } = sub {
        if let Ok(addr) = user.parse::<alloy::primitives::Address>() {
            let time = batch.block_time();
            let height = batch.block_number();
            let updates: Vec<OrderUpdate> = batch
                .clone()
                .events()
                .into_iter()
                .filter(|s| s.user == addr)
                .map(|s| OrderUpdate::new(s.user, time, height, s))
                .collect();
            if !updates.is_empty() {
                send_socket_message(socket, ServerResponse::OrderUpdates(updates)).await;
            }
        }
    }
}

impl Subscription {
    async fn handle_immediate_snapshot(
        &self,
        listener: Arc<Mutex<OrderBookListener>>,
    ) -> Result<Option<ServerResponse>> {
        if let Self::L4Book { coin } = self {
            if let Some(TimedSnapshots { time, height, snapshot }) = listener.lock().await.compute_snapshot() {
                let c = Coin::new(coin);
                if let Some((_, s)) = snapshot.value().into_iter().find(|(found, _)| *found == c) {
                    let lvls = s.as_ref().clone().map(|v| v.into_iter().map(L4Order::from).collect());
                    return Ok(Some(ServerResponse::L4Book(L4Book::Snapshot {
                        coin: coin.clone(),
                        time,
                        height,
                        levels: lvls,
                    })));
                }
            }
            return Err("Snapshot Failed".into());
        }
        Ok(None)
    }
}
