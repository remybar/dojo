pub impl ContractAddressDefault of Default<core::starknet::ContractAddress> {
    fn default() -> core::starknet::ContractAddress {
        core::starknet::contract_address_const::<0>()
    }
}
