use wincode::{SchemaRead, SchemaWrite};

#[derive(SchemaWrite, SchemaRead, Debug)]
pub struct TransactionPacket {
    pub wire_transaction: Vec<u8>,
    pub mev_protect: bool,
    pub max_retry: Option<u16>,
}
