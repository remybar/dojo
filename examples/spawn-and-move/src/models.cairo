use starknet::ContractAddress;

#[derive(Copy, Drop, Serde, Introspect, PartialEq)]
pub struct PlayerItem {
    pub item_id: u32,
    pub quantity: u32,
    pub score: i32,
}

#[derive(Drop, Serde)]
#[dojo::model]
pub struct PlayerConfig {
    #[key]
    pub player: ContractAddress,
    pub name: ByteArray,
    pub items: Array<PlayerItem>,
    pub favorite_item: Option<u32>,
}