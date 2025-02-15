// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::{
    error::{Error, Result},
    Client, ClientEvent, ClientEventsChannel, ClientEventsReceiver, Register, RegisterOffline,
};

use crate::{
    domain::client_transfers::SpendRequest,
    network::{close_group_majority, NetworkEvent, SwarmDriver, CLOSE_GROUP_SIZE},
    protocol::{
        messages::{Cmd, CmdResponse, Query, QueryResponse, Request, Response, SpendQuery},
        storage::{Chunk, ChunkAddress, DbcAddress},
        NetworkAddress,
    },
};

use sn_dbc::{DbcId, SignedSpend};

use bls::{PublicKey, SecretKey, Signature};
use futures::future::select_all;
use itertools::Itertools;
use libp2p::{kad::RecordKey, Multiaddr, PeerId};
use tokio::task::spawn;
use tracing::trace;
use xor_name::XorName;

impl Client {
    /// Instantiate a new client.
    pub async fn new(signer: SecretKey, peers: Option<Vec<(PeerId, Multiaddr)>>) -> Result<Self> {
        info!("Starting Kad swarm in client mode...");
        let (network, mut network_event_receiver, swarm_driver) = SwarmDriver::new_client()?;
        info!("Client constructed network and swarm_driver");
        let events_channel = ClientEventsChannel::default();
        let client = Self {
            network: network.clone(),
            events_channel,
            signer,
        };

        let mut must_dial_network = true;

        let mut client_clone = client.clone();

        let _swarm_driver = spawn({
            trace!("Starting up client swarm_driver");
            swarm_driver.run()
        });
        let _event_handler = spawn(async move {
            loop {
                if let Some(peers) = peers.clone() {
                    if must_dial_network {
                        let network = network.clone();
                        let _handle = spawn(async move {
                            trace!("Client dialing network");
                            for (peer_id, addr) in peers {
                                let _ = network.add_to_routing_table(peer_id, addr.clone()).await;
                                if let Err(err) = network.dial(peer_id, addr.clone()).await {
                                    tracing::error!("Failed to dial {peer_id}: {err:?}");
                                };
                            }
                        });

                        must_dial_network = false;
                    }
                }

                info!("Client waiting for a network event");
                let event = match network_event_receiver.recv().await {
                    Some(event) => event,
                    None => {
                        error!("The `NetworkEvent` channel has been closed");
                        continue;
                    }
                };
                trace!("Client recevied a network event {event:?}");
                if let Err(err) = client_clone.handle_network_event(event) {
                    warn!("Error handling network event: {err}");
                }
            }
        });

        // Wait till client confirmed with connected to enough nodes.
        let mut client_events_rx = client.events_channel();
        let mut added_node = 0;
        while added_node <= CLOSE_GROUP_SIZE {
            if let Ok(event) = client_events_rx.recv().await {
                match event {
                    ClientEvent::ConnectedToNetwork => {
                        added_node += 1;
                        info!("Client connected to the Network with {added_node:?} nodes added");
                    }
                }
            }
        }

        Ok(client)
    }

    fn handle_network_event(&mut self, event: NetworkEvent) -> Result<()> {
        match event {
            // Clients do not handle requests.
            NetworkEvent::RequestReceived { .. } => {}
            // We do not listen on sockets.
            NetworkEvent::NewListenAddr(_) => {}
            NetworkEvent::PeerAdded(peer_id) => {
                self.events_channel
                    .broadcast(ClientEvent::ConnectedToNetwork);

                let key = NetworkAddress::from_peer(peer_id);
                let network = self.network.clone();
                let _handle = spawn(async move {
                    trace!("On PeerAdded({peer_id:?}) Getting closest peers for target {key:?}");
                    let result = network.client_get_closest_peers(&key).await;
                    trace!("For target {key:?}, get closest peers {result:?}");
                });
            }
        }

        Ok(())
    }

    /// Get the client events channel.
    pub fn events_channel(&self) -> ClientEventsReceiver {
        self.events_channel.subscribe()
    }

    /// Sign the given data
    pub fn sign(&self, data: &[u8]) -> Signature {
        self.signer.sign(data)
    }

    /// Return the publick key of the data signing key
    pub fn signer_pk(&self) -> PublicKey {
        self.signer.public_key()
    }

    /// Retrieve a Register from the network.
    pub async fn get_register(&self, xorname: XorName, tag: u64) -> Result<Register> {
        info!("Retrieving a Register replica with name {xorname} and tag {tag}");
        Register::retrieve(self.clone(), xorname, tag).await
    }

    /// Create a new Register.
    pub async fn create_register(&self, xorname: XorName, tag: u64) -> Result<Register> {
        info!("Instantiating a new Register replica with name {xorname} and tag {tag}");
        Register::create(self.clone(), xorname, tag).await
    }

    /// Create a new offline Register instance.
    /// It returns a Rgister instance which can be used to apply operations offline,
    /// and publish them all to the network on a ad hoc basis.
    pub fn create_register_offline(&self, xorname: XorName, tag: u64) -> Result<RegisterOffline> {
        info!("Instantiating a new (offline) Register replica with name {xorname} and tag {tag}");
        RegisterOffline::create(self.clone(), xorname, tag)
    }

    /// Store `Chunk` to its close group.
    pub(super) async fn store_chunk(&self, chunk: Chunk) -> Result<()> {
        info!("Store chunk: {:?}", chunk.address());
        let request = Request::Cmd(Cmd::StoreChunk(chunk));
        let responses = self.send_to_closest(request).await?;

        let all_oks = responses
            .iter()
            .filter(|resp| matches!(resp, Ok(Response::Cmd(CmdResponse::StoreChunk(Ok(()))))))
            .count();
        if all_oks >= close_group_majority() {
            return Ok(());
        }

        // If there no majority OK, we will return the first error sent to us.
        for resp in responses.iter().flatten() {
            if let Response::Cmd(CmdResponse::StoreChunk(result)) = resp {
                result.clone()?;
            };
        }

        // If there were no success or fail to the expected query,
        // we check if there were any send errors.
        for resp in responses {
            let _ = resp?;
        }

        // If there were no store chunk errors, then we had unexpected responses.
        Err(Error::UnexpectedResponses)
    }

    /// Retrieve a `Chunk` from the kad network.
    pub(super) async fn get_chunk(&self, address: ChunkAddress) -> Result<Chunk> {
        info!("Getting chunk: {address:?}");
        let xorname = address.name();
        match self
            .network
            .get_provided_data(RecordKey::new(xorname))
            .await?
        {
            Ok(QueryResponse::GetChunk(result)) => Ok(result?),
            Ok(other) => {
                warn!("On querying chunk {xorname:?} received unexpected response {other:?}",);
                Err(Error::UnexpectedResponses)
            }
            Err(err) => {
                warn!("Local internal error when trying to query chunk {xorname:?}: {err:?}",);
                Err(err.into())
            }
        }
    }

    pub(crate) async fn send_to_closest(&self, request: Request) -> Result<Vec<Result<Response>>> {
        let responses = self
            .network
            .client_send_to_closest(&request)
            .await?
            .into_iter()
            .map(|res| res.map_err(Error::Network))
            .collect_vec();
        Ok(responses)
    }

    pub(crate) async fn expect_closest_majority_ok(&self, spend: SpendRequest) -> Result<()> {
        let dbc_id = spend.signed_spend.dbc_id();
        let network_address = NetworkAddress::from_dbc_address(DbcAddress::from_dbc_id(dbc_id));

        trace!("Getting the closest peers to {dbc_id:?} / {network_address:?}.");
        let closest_peers = self
            .network
            .client_get_closest_peers(&network_address)
            .await?;

        let cmd = Cmd::SpendDbc {
            signed_spend: Box::new(spend.signed_spend),
            parent_tx: Box::new(spend.parent_tx),
        };

        trace!("Sending {:?} to the closest peers.", cmd);

        let mut list_of_futures = vec![];
        for peer in closest_peers {
            let request = Request::Cmd(cmd.clone());
            let future = Box::pin(self.network.send_request(request, peer));
            list_of_futures.push(future);
        }

        let mut ok_responses = 0;

        while !list_of_futures.is_empty() {
            match select_all(list_of_futures).await {
                (Ok(Response::Cmd(CmdResponse::Spend(Ok(())))), _, remaining_futures) => {
                    trace!("Spend Ok response got.");
                    ok_responses += 1;

                    // Return once we got required number of expected responses.
                    if ok_responses >= close_group_majority() {
                        return Ok(());
                    }

                    list_of_futures = remaining_futures;
                }
                (Ok(other), _, remaining_futures) => {
                    trace!("Unexpected response got: {other}.");
                    list_of_futures = remaining_futures;
                }
                (Err(err), _, remaining_futures) => {
                    trace!("Network error: {err:?}.");
                    list_of_futures = remaining_futures;
                }
            }
        }

        Err(Error::CouldNotVerifyTransfer(format!(
            "Not enough close group nodes accepted the spend. Got {}, required: {}.",
            ok_responses,
            close_group_majority()
        )))
    }

    pub(crate) async fn expect_closest_majority_same(&self, dbc_id: &DbcId) -> Result<SignedSpend> {
        let address = DbcAddress::from_dbc_id(dbc_id);
        let network_address = NetworkAddress::from_dbc_address(address);
        trace!("Getting the closest peers to {dbc_id:?} / {network_address:?}.");
        let closest_peers = self
            .network
            .client_get_closest_peers(&network_address)
            .await?;

        let query = Query::Spend(SpendQuery::GetDbcSpend(address));
        trace!("Sending {:?} to the closest peers.", query);

        let mut list_of_futures = vec![];
        for peer in closest_peers {
            let request = Request::Query(query.clone());
            let future = Box::pin(self.network.send_request(request, peer));
            list_of_futures.push(future);
        }

        let mut ok_responses = vec![];

        while !list_of_futures.is_empty() {
            match select_all(list_of_futures).await {
                (
                    Ok(Response::Query(QueryResponse::GetDbcSpend(Ok(received_spend)))),
                    _,
                    remaining_futures,
                ) => {
                    if dbc_id == received_spend.dbc_id() {
                        trace!("Signed spend got from network.");
                        ok_responses.push(received_spend);
                    }

                    // Return once we got required number of expected responses.
                    if ok_responses.len() >= close_group_majority() {
                        use itertools::*;
                        let majority_agreement = ok_responses
                            .clone()
                            .into_iter()
                            .map(|x| (x, 1))
                            .into_group_map()
                            .into_iter()
                            .filter(|(_, v)| v.len() >= close_group_majority())
                            .max_by_key(|(_, v)| v.len())
                            .map(|(k, _)| k);

                        if let Some(agreed_spend) = majority_agreement {
                            // Majority of nodes in the close group returned the same spend of the requested id.
                            // We return the spend, so that it can be compared to the spends we have in the DBC.
                            return Ok(agreed_spend);
                        }
                    }

                    list_of_futures = remaining_futures;
                }
                (Ok(other), _, remaining_futures) => {
                    trace!("Unexpected response got: {other}.");
                    list_of_futures = remaining_futures;
                }
                (Err(err), _, remaining_futures) => {
                    trace!("Network error: {err:?}.");
                    list_of_futures = remaining_futures;
                }
            }
        }

        Err(Error::CouldNotVerifyTransfer(format!(
            "Not enough close group nodes returned the requested spend. Got {}, required: {}.",
            ok_responses.len(),
            close_group_majority()
        )))
    }
}
