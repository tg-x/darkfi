use super::Participant;
use crate::{
    crypto::{address::Address, schnorr::Signature},
    util::serial::{SerialDecodable, SerialEncodable},
};

/// This struct represents [`Block`](super::Block) information used by the Ouroboros
/// Praos consensus protocol.
#[derive(Debug, Clone, PartialEq, Eq, SerialEncodable, SerialDecodable)]
pub struct Metadata {
    /// Proof that the stakeholder is the block owner
    pub proof: String,
    /// Random seed for VRF
    pub rand_seed: String,
    /// Block owner signature
    pub signature: Signature,
    /// Block owner address
    pub address: Address,
    /// Nodes participating in the consensus process
    pub participants: Vec<Participant>,
}

impl Metadata {
    pub fn new(
        proof: String,
        rand_seed: String,
        signature: Signature,
        address: Address,
        participants: Vec<Participant>,
    ) -> Self {
        Self { proof, rand_seed, signature, address, participants }
    }
}
