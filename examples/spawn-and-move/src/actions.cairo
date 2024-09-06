#[dojo::interface]
pub trait IActions {
    fn spawn(ref world: IWorldDispatcher);
}

#[dojo::contract]
pub mod actions {
    use super::IActions;

    use starknet::{ContractAddress, get_caller_address};

    // impl: implement functions specified in trait
    #[abi(embed_v0)]
    impl ActionsImpl of IActions<ContractState> {
        fn spawn(ref world: IWorldDispatcher) {
        }
    }
}
