use crate::{
    SnapshotMode,
    listeners::order_book::{L2SnapshotParams, L2Snapshots},
    order_book::{Snapshot, multi_book::OrderBooks, types::InnerOrder},
    prelude::*,
    types::{
        inner::InnerLevel,
        node_data::{Batch, NodeDataFill, NodeDataOrderDiff, NodeDataOrderStatus},
    },
};
use log::{error, info};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub(super) struct SnapshotConfig {
    pub mode: SnapshotMode,
    pub docker_container: String,
    pub hlnode_binary: String,
    pub abci_state_path: Option<PathBuf>,
    pub snapshot_output_path: Option<PathBuf>,
    pub visor_state_path: Option<PathBuf>,
    pub data_dir: PathBuf,
}

async fn run_hl_node_cmd(cmd: &mut Command) -> Result<()> {
    let output = cmd.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        error!("hl-node command failed.\nSTDOUT: {}\nSTDERR: {}", stdout, stderr);
        return Err("hl-node execution failed".into());
    }
    Ok(())
}

pub(super) async fn process_rmp_file(config: &SnapshotConfig) -> Result<PathBuf> {
    info!("Triggering L4 snapshot (mode: {:?})", config.mode);

    let parent_dir = config.data_dir.parent().unwrap_or(&config.data_dir);
    let output_path: PathBuf;

    match config.mode {
        SnapshotMode::Docker => {
            output_path = config.snapshot_output_path.clone().unwrap_or_else(|| parent_dir.join("snapshot.json"));

            let mut cmd = Command::new("docker");
            cmd.args(&[
                "exec",
                &config.docker_container,
                "./hl-node",
                "--chain",
                "Mainnet",
                "compute-l4-snapshots",
                "--include-users",
                "hl/hyperliquid_data/abci_state.rmp",
                "hl/snapshot.json",
            ]);
            run_hl_node_cmd(&mut cmd).await?;
        }
        SnapshotMode::Direct => {
            let abci_path = config
                .abci_state_path
                .clone()
                .unwrap_or_else(|| config.data_dir.join("hl/hyperliquid_data/abci_state.rmp"));
            output_path = config.snapshot_output_path.clone().unwrap_or_else(|| PathBuf::from("/tmp/hl_snapshot.json"));

            let mut cmd = Command::new(&config.hlnode_binary);
            cmd.args(&["--chain", "Mainnet", "compute-l4-snapshots", "--include-users"])
                .arg(abci_path)
                .arg(&output_path);
            run_hl_node_cmd(&mut cmd).await?;
        }
    }

    if output_path.exists() {
        info!("Snapshot file found at: {:?}", output_path);
        Ok(output_path)
    } else {
        if let Some(parent) = output_path.parent() {
            if let Ok(entries) = fs::read_dir(parent) {
                for entry in entries.flatten() {
                    error!("Found sibling: {:?}", entry.path());
                }
            }
        }
        Err("Snapshot file not created".into())
    }
}

pub(super) fn get_visor_path(config: &SnapshotConfig) -> PathBuf {
    config.visor_state_path.clone().unwrap_or_else(|| {
        config.data_dir.parent().unwrap_or(&config.data_dir).join("hyperliquid_data/visor_abci_state.json")
    })
}

impl L2SnapshotParams {
    pub(crate) const fn new(n_sig_figs: Option<u32>, mantissa: Option<u64>) -> Self {
        Self { n_sig_figs, mantissa }
    }
}

pub(super) fn compute_l2_snapshots<O: InnerOrder + Send + Sync>(order_books: &OrderBooks<O>) -> L2Snapshots {
    L2Snapshots(
        order_books
            .as_ref()
            .par_iter()
            .map(|(coin, order_book)| {
                let mut map = HashMap::with_capacity(8);

                let base_params = L2SnapshotParams { n_sig_figs: None, mantissa: None };
                let base_snapshot = order_book.to_l2_snapshot(None, None, None);

                let mut last_snapshot = base_snapshot.clone();
                map.insert(base_params, base_snapshot);

                for n_sig_figs in (2..=5).rev() {
                    if n_sig_figs == 5 {
                        for mantissa in [None, Some(2), Some(5)] {
                            let params = L2SnapshotParams { n_sig_figs: Some(n_sig_figs), mantissa };
                            let snapshot = last_snapshot.to_l2_snapshot(None, Some(n_sig_figs), mantissa);
                            if mantissa.is_none() {
                                last_snapshot = snapshot.clone();
                            }
                            map.insert(params, snapshot);
                        }
                    } else {
                        let params = L2SnapshotParams { n_sig_figs: Some(n_sig_figs), mantissa: None };
                        let snapshot = last_snapshot.to_l2_snapshot(None, Some(n_sig_figs), None);
                        last_snapshot = snapshot.clone();
                        map.insert(params, snapshot);
                    }
                }
                (coin.clone(), map)
            })
            .collect(),
    )
}

pub(super) enum EventBatch {
    Orders(Batch<NodeDataOrderStatus>),
    BookDiffs(Batch<NodeDataOrderDiff>),
    Fills(Batch<NodeDataFill>),
}
