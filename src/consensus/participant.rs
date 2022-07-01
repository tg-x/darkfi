use std::{collections::BTreeMap, io};

use crate::{
    crypto::{address::Address, keypair::PublicKey, schnorr::Signature},
    impl_vec, net,
    util::serial::{Decodable, Encodable, SerialDecodable, SerialEncodable, VarInt},
    Result,
};

/// This struct represents a tuple of the form:
/// (`public_key`, `node_address`, `last_slot_seen`,`slot_quarantined`)
#[derive(Debug, Clone, PartialEq, Eq, SerialEncodable, SerialDecodable)]
pub struct Participant {
    /// Node public key
    pub public_key: PublicKey,
    /// Node wallet address
    pub address: Address,
    /// Last slot node send a keep alive message
    pub seen: u64,
    /// Slot participant was quarantined by the node
    pub quarantined: Option<u64>,
}

impl Participant {
    pub fn new(public_key: PublicKey, address: Address, joined: u64) -> Self {
        Self { public_key, address, seen: joined, quarantined: None }
    }
}

impl net::Message for Participant {
    fn name() -> &'static str {
        "participant"
    }
}

impl Encodable for BTreeMap<Address, Participant> {
    fn encode<S: io::Write>(&self, mut s: S) -> Result<usize> {
        let mut len = 0;
        len += VarInt(self.len() as u64).encode(&mut s)?;
        for c in self.iter() {
            len += c.1.encode(&mut s)?;
        }
        Ok(len)
    }
}

impl Decodable for BTreeMap<Address, Participant> {
    fn decode<D: io::Read>(mut d: D) -> Result<Self> {
        let len = VarInt::decode(&mut d)?.0;
        let mut ret = BTreeMap::new();
        for _ in 0..len {
            let participant: Participant = Decodable::decode(&mut d)?;
            ret.insert(participant.address, participant);
        }
        Ok(ret)
    }
}

impl_vec!(Participant);

/// Struct represending a keep alive message, containing signed slot for validation
#[derive(Debug, Clone, SerialEncodable, SerialDecodable)]
pub struct KeepAlive {
    /// Node address
    pub address: Address,
    /// Slot message was send
    pub slot: u64,
    /// Slot signature
    pub signature: Signature,
}

impl net::Message for KeepAlive {
    fn name() -> &'static str {
        "keepalive"
    }
}
