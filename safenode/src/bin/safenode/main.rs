// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.
mod rpc;

use safenode::{
    log::init_node_logging,
    node::{Node, NodeEvent, NodeEventsReceiver},
};

use clap::Parser;
use eyre::{eyre, Error, Result};
use libp2p::{multiaddr::Protocol, Multiaddr, PeerId};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    runtime::Runtime,
    sync::{broadcast::error::RecvError, mpsc},
    time::sleep,
};
use tracing::{error, info, warn};

// Please do not remove the blank lines in these doc comments.
// They are used for inserting line breaks when the help menu is rendered in the UI.
#[derive(Parser, Debug)]
#[clap(name = "safenode cli")]
struct Opt {
    /// Specify the node's logging output directory.
    ///
    /// If not provided, logging will go to stdout.
    #[clap(long)]
    log_dir: Option<PathBuf>,

    /// Specify the node's data directory.
    ///
    /// If not provided, the default location is platform specific:
    ///
    /// * Linux: $HOME/.local/share/safe/node
    ///
    /// * macOS: $HOME/Library/Application Support/safe/node
    ///
    /// * Windows: C:\Users\{username}\AppData\Roaming\safe\node
    #[clap(long)]
    root_dir: Option<PathBuf>,

    /// Specify the port to listen on.
    ///
    /// Defaults to 0, which means any available port.
    #[clap(long, default_value_t = 0)]
    port: u16,

    /// Specify the IP to listen on.
    ///
    /// Defaults to 0.0.0.0, which will bind to all network interfaces.
    #[clap(long, default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
    ip: IpAddr,

    /// Provide a peer to connect to a public network, using the MultiAddr format.
    ///
    /// The MultiAddr format looks like so:
    ///
    /// /ip4/13.40.152.226/udp/12000/quic-v1/p2p/12D3KooWRi6wF7yxWLuPSNskXc6kQ5cJ6eaymeMbCRdTnMesPgFx
    ///
    /// Noteworthy are the second, fourth, and last parts.
    ///
    /// Those are the IP address and UDP port the peer is listening on, and its peer ID, respectively.
    ///
    /// Many peers can be provided by using the argument multiple times.
    ///
    /// If none are provided, a connection will be attempted to a local network.
    #[clap(long = "peer", value_name = "MultiAddr")]
    peers: Vec<Multiaddr>,

    /// Enable the admin/ctrl RPC service by providing an IP and port for it to listen on.
    #[clap(long)]
    rpc: Option<SocketAddr>,
}

#[derive(Debug)]
// To be sent to the main thread in order to stop/restart the execution of the safenode app.
enum NodeCtrl {
    // Request to stop the exeution of the safenode app, providing an error as a reason for it.
    Stop { delay: Duration, cause: Error },
    // Request to restart the exeution of the safenode app,
    // retrying to join the network, after the requested delay.
    Restart(Duration),
    // Request to update the safenode app, and restart it, after the requested delay.
    Update(Duration),
}

fn main() -> Result<()> {
    let opt = Opt::parse();
    #[cfg(not(feature = "otlp"))]
    let _log_appender_guard = init_node_logging(&opt.log_dir)?;
    #[cfg(feature = "otlp")]
    let (_rt, _guard) = {
        // init logging in a separate runtime if we are sending traces to an opentelemetry server
        let rt = Runtime::new()?;
        let guard = rt.block_on(async { init_node_logging(&opt.log_dir) })?;
        (rt, guard)
    };

    let root_dir = get_root_dir_path(opt.root_dir)?;
    let log_dir = if let Some(path) = opt.log_dir {
        format!("{}", path.display())
    } else {
        "stdout".to_string()
    };

    let node_socket_addr = SocketAddr::new(opt.ip, opt.port);
    let peers = parse_peer_multiaddreses(&opt.peers)?;

    loop {
        let msg = format!(
            "Running {} v{}",
            env!("CARGO_BIN_NAME"),
            env!("CARGO_PKG_VERSION")
        );
        info!("\n{}\n{}", msg, "=".repeat(msg.len()));

        // Create a tokio runtime per `start_node` attempt, this ensures
        // any spawned tasks are closed before this would be run again.
        let rt = Runtime::new()?;
        rt.block_on(start_node(
            node_socket_addr,
            peers.clone(),
            opt.rpc,
            &log_dir,
            &root_dir,
        ))?;

        // actively shut down the runtime
        rt.shutdown_timeout(Duration::from_secs(2));
    }
}

async fn start_node(
    node_socket_addr: SocketAddr,
    peers: Vec<(PeerId, Multiaddr)>,
    rpc: Option<SocketAddr>,
    log_dir: &str,
    root_dir: &Path,
) -> Result<()> {
    let started_instant = std::time::Instant::now();

    info!("Starting node ...");
    let running_node = Node::run(node_socket_addr, peers, root_dir).await?;

    // Channel to receive node ctrl cmds from RPC service (if enabled), and events monitoring task
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<NodeCtrl>(5);

    // Monitor `NodeEvents`
    let node_events_rx = running_node.node_events_channel().subscribe();
    monitor_node_events(node_events_rx, ctrl_tx.clone());

    // Start up gRPC interface if enabled by user
    if let Some(addr) = rpc {
        rpc::start_rpc_service(addr, log_dir, running_node, ctrl_tx, started_instant);
    }

    // Keep the node and gRPC service (if enabled) running.
    // We'll monitor any NodeCtrl cmd to restart/stop/update,
    loop {
        match ctrl_rx.recv().await {
            Some(NodeCtrl::Restart(delay)) => {
                let msg = format!("Node is restarting in {delay:?}...");
                info!("{msg}");
                println!("{msg} Node log path: {log_dir}");
                sleep(delay).await;
                break Ok(());
            }
            Some(NodeCtrl::Stop { delay, cause }) => {
                let msg = format!("Node is stopping in {delay:?}...");
                info!("{msg}");
                println!("{msg} Node log path: {log_dir}");
                sleep(delay).await;
                return Err(cause);
            }
            Some(NodeCtrl::Update(_delay)) => {
                // TODO: implement self-update once safenode app releases are published again
                println!("No self-update supported yet.");
            }
            None => {
                info!("Internal node ctrl cmds channel has been closed, restarting node");
                break Ok(());
            }
        }
    }
}

fn monitor_node_events(mut node_events_rx: NodeEventsReceiver, ctrl_tx: mpsc::Sender<NodeCtrl>) {
    let _handle = tokio::spawn(async move {
        loop {
            match node_events_rx.recv().await {
                Ok(NodeEvent::ConnectedToNetwork) => info!("Connected to the Network"),
                Ok(_) => { /* we ignore other evvents */ }
                Err(RecvError::Closed) => {
                    if let Err(err) = ctrl_tx
                        .send(NodeCtrl::Stop {
                            delay: Duration::from_secs(1),
                            cause: eyre!("Node events channel closed!"),
                        })
                        .await
                    {
                        error!(
                            "Failed to send node control msg to safenode bin main thread: {err}"
                        );
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    warn!("Skipped {n} node events!");
                    continue;
                }
            }
        }
    });
}

/// Parse multiaddresses containing the P2p protocol (`/p2p/<PeerId>`).
/// Returns an error for the first invalid multiaddress.
fn parse_peer_multiaddreses(multiaddrs: &[Multiaddr]) -> Result<Vec<(PeerId, Multiaddr)>> {
    multiaddrs
        .iter()
        .map(|multiaddr| {
            // Take hash from the `/p2p/<hash>` component.
            let p2p_multihash = multiaddr
                .iter()
                .find_map(|p| match p {
                    Protocol::P2p(hash) => Some(hash),
                    _ => None,
                })
                .ok_or_else(|| eyre!("address does not contain `/p2p/<PeerId>`"))?;
            // Parse the multihash into the `PeerId`.
            let peer_id =
                PeerId::from_multihash(p2p_multihash).map_err(|_| eyre!("invalid p2p PeerId"))?;

            Ok((peer_id, multiaddr.clone()))
        })
        // Short circuit on the first error. See rust docs `Result::from_iter`.
        .collect::<Result<Vec<(PeerId, Multiaddr)>>>()
}

fn get_root_dir_path(root_dir_path: Option<PathBuf>) -> Result<PathBuf> {
    let path = if let Some(path) = root_dir_path {
        path
    } else {
        dirs_next::data_dir()
            .ok_or_else(|| eyre!("Could not obtain data directory"))?
            .join("safe")
            .join("node")
    };
    std::fs::create_dir_all(path.clone())?;
    Ok(path)
}
