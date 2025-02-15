// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#[macro_use]
extern crate tracing;

mod cli;

use self::cli::{files_cmds, register_cmds, wallet_cmds, Opt, SubCmd};

use clap::Parser;
use eyre::{eyre, Result};
use libp2p::{multiaddr::Protocol, Multiaddr, PeerId};
use safenode::client::Client;
use safenode::log::init_node_logging;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::parse();
    // For client, default to log to std::out
    // This is ruining the log output for the CLI. Needs to be fixed.
    let tmp_dir = std::env::temp_dir();
    let _log_appender_guard = init_node_logging(&Some(tmp_dir.join("safe-client.log")))?;

    info!("Full client logs will be written to {:?}", tmp_dir);

    println!("Instantiating a SAFE client...");

    let secret_key = bls::SecretKey::random();
    let peers = parse_peer_multiaddresses(&opt.peers)?;

    let client = Client::new(secret_key, Some(peers)).await?;

    let root_dir = get_client_dir().await?;

    match opt.cmd {
        SubCmd::Wallet(cmds) => wallet_cmds(cmds, &client, &root_dir).await?,
        SubCmd::Files(cmds) => files_cmds(cmds, client.clone(), &root_dir).await?,
        SubCmd::Register(cmds) => register_cmds(cmds, &client).await?,
    };

    Ok(())
}

async fn get_client_dir() -> Result<PathBuf> {
    let mut home_dirs = dirs_next::home_dir().expect("A homedir to exist.");
    home_dirs.push(".safe");
    home_dirs.push("client");
    tokio::fs::create_dir_all(home_dirs.as_path()).await?;
    Ok(home_dirs)
}

// TODO: dedupe
/// Parse multiaddresses containing the P2p protocol (`/p2p/<PeerId>`).
/// Returns an error for the first invalid multiaddress.
fn parse_peer_multiaddresses(multiaddrs: &[Multiaddr]) -> Result<Vec<(PeerId, Multiaddr)>> {
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
