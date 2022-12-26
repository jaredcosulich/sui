#[test_only]
module capy::capy_tests {
    use sui::test_scenario::{Self};
    use capy::capy;
    use capy::capy::{CapyManagerCap, CapyRegistry, Capy};
    use sui::transfer;

    const ADMIN: address = @0x123;
    const USER: address = @0x234;

    #[test]
    fun test_init() {
        let scenario = test_scenario::begin(ADMIN);
        {
            let ctx = test_scenario::ctx(&mut scenario);
            capy::init_for_test(ctx)
        };

        test_scenario::end(scenario);
    }

    #[test]
    fun test_batch() {
        let scenario = test_scenario::begin(ADMIN);
        {
            let ctx = test_scenario::ctx(&mut scenario);
            capy::init_for_test(ctx)
        };

        test_scenario::next_tx(&mut scenario, ADMIN);
        {
            let capy_manager_cap = test_scenario::take_from_sender<CapyManagerCap>(&mut scenario);
            let reg = test_scenario::take_shared<CapyRegistry>(&mut scenario);
            capy::batch_for_test(&capy_manager_cap, &mut reg, &mut scenario);
            test_scenario::return_to_sender(&scenario, capy_manager_cap);
            test_scenario::return_shared(reg);
        };

        test_scenario::end(scenario);
    }

    #[test]
    fun test_transfer_capy() {
        let scenario = test_scenario::begin(ADMIN);
        {
            let ctx = test_scenario::ctx(&mut scenario);
            capy::init_for_test(ctx)
        };

        test_scenario::next_tx(&mut scenario, ADMIN);
        {
            let capy_manager_cap = test_scenario::take_from_sender<CapyManagerCap>(&mut scenario);
            let reg = test_scenario::take_shared<CapyRegistry>(&mut scenario);
            capy::batch_for_test(&capy_manager_cap, &mut reg, &mut scenario);
            test_scenario::return_to_sender(&scenario, capy_manager_cap);
            test_scenario::return_shared(reg);
        };

        test_scenario::next_tx(&mut scenario, ADMIN);
        {
            let capy = test_scenario::take_from_sender<Capy>(&mut scenario);
            transfer::transfer(capy, USER)
        };

        test_scenario::next_tx(&mut scenario, ADMIN);
        {
            let capy = test_scenario::take_from_sender<Capy>(&mut scenario);
            transfer::transfer(capy, USER)
        };

        test_scenario::end(scenario);

    }
}