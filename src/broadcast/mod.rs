//! Reliable broadcast algorithm instance.
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use crossbeam;
use proto::*;
use std::marker::{Send, Sync};
use merkle::{Hashable, MerkleTree};
use merkle::proof::{Proof, Lemma, Positioned};
use reed_solomon_erasure as rse;
use reed_solomon_erasure::ReedSolomon;
use crossbeam_channel::{Sender, Receiver, SendError, RecvError};

use messaging::{Target, TargetedMessage, SourcedMessage};

type MerkleValue = Vec<u8>;

/// Broadcast algorithm instance.
///
/// The ACS algorithm requires multiple broadcast instances running
/// asynchronously, see Figure 4 in the HBBFT paper. Those are N asynchronous
/// coroutines, each responding to values from one particular remote node. The
/// paper doesn't make it clear though how other messages - Echo and Ready - are
/// distributed over the instances. Also it appears that the sender of a message
/// might become part of the message for this to work.
pub struct Instance<'a, T: 'a + Clone + Debug + Send + Sync> {
    /// The transmit side of the channel to comms threads.
    tx: &'a Sender<TargetedMessage<MerkleValue>>,
    /// The receive side of the channel from comms threads.
    rx: &'a Receiver<SourcedMessage<MerkleValue>>,
    /// Value to be broadcast.
    broadcast_value: Option<T>,
    /// This instance's index for identification against its comms task.
    node_index: usize,
    /// Number of nodes participating in broadcast.
    num_nodes: usize,
    /// Maximum allowed number of faulty nodes.
    num_faulty_nodes: usize
}

impl<'a, T: Clone + Debug + Hashable + Send + Sync
     + Into<Vec<u8>> + From<Vec<u8>>>
    Instance<'a, T>
{
    pub fn new(tx: &'a Sender<TargetedMessage<MerkleValue>>,
               rx: &'a Receiver<SourcedMessage<MerkleValue>>,
               broadcast_value: Option<T>,
               num_nodes: usize,
               node_index: usize) ->
        Self
    {
        Instance {
            tx: tx,
            rx: rx,
            broadcast_value: broadcast_value,
            node_index: node_index,
            num_nodes: num_nodes,
            num_faulty_nodes: (num_nodes - 1) / 3
        }
    }

    /// Broadcast stage task returning the computed values in case of success,
    /// and an error in case of failure.
    pub fn run(&mut self) -> Result<T, Error> {
        // Broadcast state machine thread.
        let bvalue = self.broadcast_value.to_owned();
        let result: Result<T, Error>;
        let result_r = Arc::new(Mutex::new(None));
        let result_r_scoped = result_r.clone();

        crossbeam::scope(|scope| {
            scope.spawn(move || {
                *result_r_scoped.lock().unwrap() =
                    Some(inner_run(self.tx, self.rx, bvalue,
                                   self.node_index, self.num_nodes,
                                   self.num_faulty_nodes));
            });
        });
        if let Some(ref r) = *result_r.lock().unwrap() {
            result = r.to_owned();
        }
        else {
            result = Err(Error::Threading);
        }
        result
    }
}

/// Errors returned by the broadcast instance.
#[derive(Debug, Clone)]
pub enum Error {
    RootHashMismatch,
    Threading,
    ProofConstructionFailed,
    ReedSolomon(rse::Error),
    Send(SendError<TargetedMessage<MerkleValue>>),
    Recv(RecvError)
}

impl From<rse::Error> for Error
{
    fn from(err: rse::Error) -> Error { Error::ReedSolomon(err) }
}

impl From<SendError<TargetedMessage<MerkleValue>>> for Error
{
    fn from(err: SendError<TargetedMessage<MerkleValue>>) -> Error { Error::Send(err) }
}

impl From<RecvError> for Error
{
    fn from(err: RecvError) -> Error { Error::Recv(err) }
}

/// Breaks the input value into shards of equal length and encodes them -- and
/// some extra parity shards -- with a Reed-Solomon erasure coding scheme. The
/// returned value contains the shard assigned to this node. That shard doesn't
/// need to be sent anywhere. It is returned to the broadcast instance and gets
/// recorded immediately.
fn send_shards<'a, T>(value: T,
                      tx: &'a Sender<TargetedMessage<MerkleValue>>,
                      coding: &ReedSolomon) ->
    Result<Proof<MerkleValue>, Error>
where T: Clone + Debug + Hashable + Send + Sync + Into<Vec<u8>> + From<Vec<u8>>
{
    let data_shard_num = coding.data_shard_count();
    let parity_shard_num = coding.parity_shard_count();

    debug!("Data shards: {}, parity shards: {}",
           data_shard_num, parity_shard_num);
    let mut v: Vec<u8> = T::into(value);
    // Insert the length of `v` so it can be decoded without the padding.
    let payload_len = v.len() as u8;
    v.insert(0, payload_len); // TODO: Handle messages larger than 255 bytes.
    let value_len = v.len();
    // Size of a Merkle tree leaf value, in bytes.
    let shard_len = if value_len % data_shard_num > 0 {
        value_len / data_shard_num + 1
    }
    else {
        value_len / data_shard_num
    };
    // Pad the last data shard with zeros. Fill the parity shards with zeros.
    v.resize(shard_len * (data_shard_num + parity_shard_num), 0);

    debug!("value_len {}, shard_len {}", value_len, shard_len);

    // Divide the vector into chunks/shards.
    let shards_iter = v.chunks_mut(shard_len);
    // Convert the iterator over slices into a vector of slices.
    let mut shards: Vec<&mut [u8]> = Vec::new();
    for s in shards_iter {
        shards.push(s);
    }

    debug!("Shards before encoding: {:?}", shards);

    // Construct the parity chunks/shards
    coding.encode(shards.as_mut_slice())?;

    debug!("Shards: {:?}", shards);

    let shards_t: Vec<MerkleValue> =
        shards.into_iter().map(|s| s.to_vec()).collect();

    // Convert the Merkle tree into a partial binary tree for later
    // deconstruction into compound branches.
    let mtree = MerkleTree::from_vec(&::ring::digest::SHA256, shards_t);

    // Default result in case of `gen_proof` error.
    let mut result = Err(Error::ProofConstructionFailed);

    // Send each proof to a node.
    for (i, leaf_value) in mtree.iter().enumerate() {
        let proof = mtree.gen_proof(leaf_value.to_vec());
        if let Some(proof) = proof {
            if i == 0 {
                // The first proof is addressed to this node.
                result = Ok(proof);
            }
            else {
                // Rest of the proofs are sent to remote nodes.
                tx.send(
                    TargetedMessage {
                        target: Target::Node(i),
                        message: Message::Broadcast(
                            BroadcastMessage::Value(proof))
                    })?;
            }
        }
    }

    result
}

/// The main loop of the broadcast task.
fn inner_run<'a, T>(tx: &'a Sender<TargetedMessage<MerkleValue>>,
                    rx: &'a Receiver<SourcedMessage<MerkleValue>>,
                    broadcast_value: Option<T>,
                    node_index: usize,
                    num_nodes: usize,
                    num_faulty_nodes: usize) ->
    Result<T, Error>
where T: Clone + Debug + Hashable + Send + Sync + Into<Vec<u8>> + From<Vec<u8>>
{
    // Erasure coding scheme: N - 2f value shards and 2f parity shards
    let parity_shard_num = 2 * num_faulty_nodes;
    let data_shard_num = num_nodes - parity_shard_num;
    let coding = ReedSolomon::new(data_shard_num, parity_shard_num)?;
    // currently known leaf values
    let mut leaf_values: Vec<Option<Box<[u8]>>> = vec![None; num_nodes];
    // Write-once root hash of a tree broadcast from the sender associated with
    // this instance.
    let mut root_hash: Option<Vec<u8>> = None;
    // number of non-None leaf values
    let mut leaf_values_num = 0;

    // Split the value into chunks/shards, encode them with erasure codes.
    // Assemble a Merkle tree from data and parity shards. Take all proofs from
    // this tree and send them, each to its own node.
    if let Some(v) = broadcast_value {
        send_shards(v, tx, &coding)
            .map(|proof| {
                // Record the first proof as if it were sent by the node to
                // itself.
                let h = proof.root_hash.clone();
                if proof.validate(h.as_slice()) {
                    // Save the leaf value for reconstructing the tree later.
                    leaf_values[index_of_proof(&proof)] =
                        Some(proof.value.clone().into_boxed_slice());
                    leaf_values_num = leaf_values_num + 1;
                    root_hash = Some(h);
                }
            })?
    }

    // return value
    let mut result: Option<Result<T, Error>> = None;
    // Number of times Echo was received with the same root hash.
    let mut echo_num = 0;
    // Number of times Ready was received with the same root hash.
    let mut readys: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut ready_sent = false;
    let mut ready_to_decode = false;

    // TODO: handle exit conditions
    while result.is_none() {
        // Receive a message from the socket IO task.
        let message = rx.recv()?;
        if let SourcedMessage {
            source: i,
            message: Message::Broadcast(message)
        } = message {
            match message {
                // A value received. Record the value and multicast an echo.
                BroadcastMessage::Value(p) => {
                    if i != node_index {
                        // Ignore value messages from unrelated remote nodes.
                        continue;
                    }

                    if let None = root_hash {
                        root_hash = Some(p.root_hash.clone());
                        debug!("Node {} Value root hash {:?}",
                               node_index, HexBytes(&p.root_hash));
                    }

                    if let &Some(ref h) = &root_hash {
                        if p.validate(h.as_slice()) {
                            // Save the leaf value for reconstructing the tree
                            // later.
                            leaf_values[index_of_proof(&p)] =
                                Some(p.value.clone().into_boxed_slice());
                            leaf_values_num = leaf_values_num + 1;
                        }
                    }
                    // Broadcast an echo of this proof.
                    tx.send(TargetedMessage {
                        target: Target::All,
                        message: Message::Broadcast(BroadcastMessage::Echo(p))
                    })?
                },

                // An echo received. Verify the proof it contains.
                BroadcastMessage::Echo(p) => {
                    if let None = root_hash {
                        if i == node_index {
                            root_hash = Some(p.root_hash.clone());
                            debug!("Node {} Echo root hash {:?}",
                                   node_index, root_hash);
                        }
                    }

                    // call validate with the root hash as argument
                    if let &Some(ref h) = &root_hash {
                        if p.validate(h.as_slice()) {
                            echo_num += 1;
                            // Save the leaf value for reconstructing the tree
                            // later.
                            leaf_values[index_of_proof(&p)] =
                                Some(p.value.clone().into_boxed_slice());
                            leaf_values_num = leaf_values_num + 1;

                            // upon receiving 2f + 1 matching READY(h)
                            // messages, wait for N − 2 f ECHO messages, then
                            // decode v
                            if ready_to_decode &&
                                leaf_values_num >=
                                num_nodes - 2 * num_faulty_nodes
                            {
                                result = Some(
                                    decode_from_shards(&mut leaf_values,
                                                       &coding,
                                                       data_shard_num, h));
                            }
                            else if leaf_values_num >=
                                num_nodes - num_faulty_nodes
                            {
                                result = Some(
                                    decode_from_shards(&mut leaf_values,
                                                       &coding,
                                                       data_shard_num, h));
                                // if Ready has not yet been sent, multicast
                                // Ready
                                if !ready_sent {
                                    ready_sent = true;
                                    tx.send(TargetedMessage {
                                        target: Target::All,
                                        message: Message::Broadcast(
                                            BroadcastMessage::Ready(
                                                h.to_owned()))
                                    })?;
                                }
                            }
                        }
                    }
                },

                BroadcastMessage::Ready(ref hash) => {
                    // Update the number Ready has been received with this hash.
                    *readys.entry(hash.to_vec()).or_insert(1) += 1;

                    // Check that the root hash matches.
                    if let &Some(ref h) = &root_hash {
                        let ready_num: usize = *readys.get(h).unwrap_or(&0);

                        // Upon receiving f + 1 matching Ready(h) messages, if
                        // Ready has not yet been sent, multicast Ready(h).
                        if (ready_num == num_faulty_nodes + 1) &&
                            !ready_sent
                        {
                            tx.send(TargetedMessage {
                                target: Target::All,
                                message: Message::Broadcast(
                                    BroadcastMessage::Ready(
                                        h.to_vec()))
                            })?;
                        }

                        // Upon receiving 2f + 1 matching Ready(h) messages,
                        // wait for N − 2f Echo messages, then decode v.
                        if ready_num > 2 * num_faulty_nodes {
                            // Wait for N - 2f Echo messages, then decode v.
                            if echo_num >= num_nodes - 2 * num_faulty_nodes {
                                result = Some(
                                    decode_from_shards(&mut leaf_values,
                                                       &coding,
                                                       data_shard_num, h));
                            }
                            else {
                                ready_to_decode = true;
                            }
                        }
                    }
                }
            }
        }
        else {
            error!("Incorrect message from the socket: {:?}", message);
        }
    }
    // result is not a None, safe to extract value
    result.unwrap()
}

fn decode_from_shards<T>(leaf_values: &mut Vec<Option<Box<[u8]>>>,
                         coding: &ReedSolomon,
                         data_shard_num: usize,
                         root_hash: &Vec<u8>) ->
    Result<T, Error>
where T: Clone + Debug + Hashable + Send + Sync + From<Vec<u8>> + Into<Vec<u8>>
{
    // Try to interpolate the Merkle tree using the Reed-Solomon erasure coding
    // scheme.
    coding.reconstruct_shards(leaf_values.as_mut_slice())?;

    // Recompute the Merkle tree root.
    //
    // Collect shards for tree construction.
    let mut shards: Vec<MerkleValue> = Vec::new();
    for l in leaf_values.iter() {
        if let Some(ref v) = *l {
            shards.push(v.to_vec());
        }
    }
    // Construct the Merkle tree.
    let mtree = MerkleTree::from_vec(&::ring::digest::SHA256, shards);
    // If the root hash of the reconstructed tree does not match the one
    // received with proofs then abort.
    if *mtree.root_hash() != *root_hash {
        // NOTE: The paper does not define the meaning of *abort*. But it is
        // sensible not to continue trying to reconstruct the tree after this
        // point. This instance must have received incorrect shards.
        Err(Error::RootHashMismatch)
    }
    else {
        // Reconstruct the value from the data shards.
        Ok(glue_shards(mtree, data_shard_num))
    }
}

/// Concatenates the first `n` leaf values of a Merkle tree `m` in one value of
/// type `T`. This is useful for reconstructing the data value held in the tree
/// and forgetting the leaves that contain parity information.
fn glue_shards<T>(m: MerkleTree<MerkleValue>, n: usize) -> T
where T: From<Vec<u8>> + Into<Vec<u8>>
{
    let mut t: Vec<u8> = Vec::new();
    let mut i = 0;

    for s in m.into_iter() {
        i += 1;
        if i > n {
            break;
        }
        for b in s {
            t.push(b);
        }
    }
    let payload_len = t[0] as usize;
    debug!("Glued data shards {:?}", &t[1..(payload_len + 1)]);

    Vec::into(t[1..(payload_len + 1)].to_vec())
}

/// An additional path conversion operation on `Lemma` to allow reconstruction
/// of erasure-coded `Proof` from `Lemma`s. The output path, when read from left
/// to right, goes from leaf to root (LSB order).
fn path_of_lemma(lemma: &Lemma) -> Vec<bool> {
    match lemma.sub_lemma {
        None => {
            match lemma.sibling_hash {
                // lemma terminates with no leaf
                None => vec![],
                // the leaf is on the right
                Some(Positioned::Left(_)) => vec![true],
                // the leaf is on the left
                Some(Positioned::Right(_)) => vec![false],
            }
        }
        Some(ref l) => {
            let mut p = path_of_lemma(l.as_ref());

            match lemma.sibling_hash {
                // lemma terminates
                None => (),
                // lemma branches out to the right
                Some(Positioned::Left(_)) => p.push(true),
                // lemma branches out to the left
                Some(Positioned::Right(_)) => p.push(false),
            }
            p
        }
    }
}

/// Further conversion of a binary tree path into an array index.
fn index_of_path(mut path: Vec<bool>) -> usize {
    let mut idx = 0;
    // Convert to the MSB order.
    path.reverse();

    for &dir in path.iter() {
        idx = idx << 1;
        if dir == true {
            idx = idx | 1;
        }
    }
    idx
}

/// Computes the Merkle tree leaf index of a value in a given proof.
fn index_of_proof(p: &Proof<MerkleValue>) -> usize {
    index_of_path(path_of_lemma(&p.lemma))
}
