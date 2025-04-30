#[derive(Copy, Drop, Serde, Introspect, Debug)]
pub enum E1 {
    X,
    Y,
    Z,
}

#[derive(Copy, Drop, Serde, Introspect, Debug)]
pub enum E2 {
    X: (u8, u32),
    Y: Option<u16>,
    Z,
}

#[derive(Copy, Drop, Serde, Introspect, Debug)]
#[dojo::model]
struct M1 {
    #[key]
    k1: u8,
    v1: E1,
    v2: E2,
    v3: Option<u8>,
}

#[derive(Copy, Drop, Serde, Introspect, Debug)]
#[dojo::model]
struct M2 {
    #[key]
    k1: u8,
    v1: E1,
    v2: E2,
    v3: Option<u8>,
}

#[starknet::interface]
pub trait IFunctions<T> {
    fn set_m1_1(ref self: T);
    fn set_m1_2(ref self: T);
    fn set_m2_1(ref self: T);
    fn set_m2_2(ref self: T);
    fn set_all(ref self: T);
}

#[dojo::contract]
pub mod functions {
    use super::{M1, M2, E1, E2};
    use dojo::model::ModelStorage;

    #[abi(embed_v0)]
    impl FunctionsImpl of super::IFunctions<ContractState> {
        fn set_m1_1(ref self: ContractState) {
            let mut world = self.world(@"ns");
            let m1 = M1 { k1: 1, v1: E1::Z, v2: E2::Y(Option::Some(123)), v3: Option::Some(42) };
            world.write_model(@m1);
        }

        fn set_m1_2(ref self: ContractState) {
            let mut world = self.world(@"ns");
            let m1 = M1 { k1: 2, v1: E1::X, v2: E2::X((123, 789)), v3: Option::None };
            world.write_model(@m1);
        }

        fn set_m2_1(ref self: ContractState) {
            let mut world = self.world(@"ns");
            let m2 = M2 { k1: 1, v1: E1::Z, v2: E2::Y(Option::Some(123)), v3: Option::Some(42) };
            world.write_model(@m2);
        }

        fn set_m2_2(ref self: ContractState) {
            let mut world = self.world(@"ns");
            let m2 = M2 { k1: 2, v1: E1::X, v2: E2::X((123, 789)), v3: Option::None };
            world.write_model(@m2);
        }

        fn set_all(ref self: ContractState) {
            self.set_m1_1();
            self.set_m1_2();
            self.set_m2_1();
            self.set_m2_2();
        }
    }
}
