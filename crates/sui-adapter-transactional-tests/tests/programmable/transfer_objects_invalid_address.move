// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

// tessts various invalid vector instantions for types

//# init --addresses test=0x0 --accounts A B

//# publish
module test::m1 {
    use sui::object::{Self, UID};
    use sui::tx_context::TxContext;

    struct Pub has key, store {
        id: UID,
        value: u64,
    }

    public fun new(ctx: &mut TxContext): Pub {
        Pub { id: object::new(ctx), value: 112 }
    }

    public fun value(): u128 {
        0
    }

    public fun vec(): vector<u8> {
        sui::address::to_bytes(@0)
    }
}

// not an address
//# programmable --sender A --inputs 0u64
//> 0: test::m1::new();
//> TransferObjects([Result(0)], Input(0));

// not an address
//# programmable --sender A
//> 0: test::m1::new();
//> 1: test::m1::value();
//> TransferObjects([Result(0)], Result(1));

// not an address
//# programmable --sender A
//> 0: test::m1::new();
//> 1: test::m1::vec();
//> TransferObjects([Result(0)], Result(1));
