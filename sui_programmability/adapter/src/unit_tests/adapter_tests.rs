// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{adapter, genesis};
use move_binary_format::file_format::{
    self, AbilitySet, AddressIdentifierIndex, IdentifierIndex, ModuleHandle, ModuleHandleIndex,
    StructHandle,
};
use move_core_types::{account_address::AccountAddress, ident_str, language_storage::StructTag};
use move_package::BuildConfig;
use std::{mem, path::PathBuf};
use sui_types::{
    base_types::{self, SequenceNumber},
    error::SuiResult,
    gas_coin::GAS,
    object::{Data, Owner},
    storage::{BackingPackageStore, Storage},
    MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS,
};

use super::*;

const GAS_BUDGET: u64 = 10000;

// temporary store where writes buffer before they get committed
#[derive(Default, Debug)]
struct ScratchPad {
    updated: BTreeMap<ObjectID, Object>,
    created: BTreeMap<ObjectID, Object>,
    deleted: BTreeMap<ObjectID, (SequenceNumber, DeleteKind)>,
    events: Vec<Event>,
    created_object_ids: HashSet<ObjectID>,
}

// TODO: We should use AuthorityTemporaryStore instead.
// Keeping this functionally identical to AuthorityTemporaryStore is a pain.
#[derive(Default, Debug)]
struct InMemoryStorage {
    persistent: BTreeMap<ObjectID, Object>,
    temporary: ScratchPad,
}

impl BackingPackageStore for InMemoryStorage {
    fn get_package(&self, package_id: &ObjectID) -> SuiResult<Option<Object>> {
        Ok(self.persistent.get(package_id).cloned())
    }
}

impl InMemoryStorage {
    pub fn new(objects: Vec<Object>) -> Self {
        let mut persistent = BTreeMap::new();
        for o in objects {
            persistent.insert(o.id(), o);
        }
        Self {
            persistent,
            temporary: ScratchPad::default(),
        }
    }

    /// Return the package that contains the module `name` (if any)
    pub fn find_package(&self, name: &str) -> Option<Object> {
        self.persistent
            .values()
            .find(|o| {
                if let Some(package) = o.data.try_as_package() {
                    if package.serialized_module_map().get(name).is_some() {
                        return true;
                    }
                }
                false
            })
            .cloned()
    }

    /// Flush writes in scratchpad to persistent storage
    pub fn flush(&mut self) {
        let to_flush = mem::take(&mut self.temporary);
        for (id, o) in to_flush.created {
            assert!(self.persistent.insert(id, o).is_none())
        }
        for (id, o) in to_flush.updated {
            assert!(self.persistent.insert(id, o).is_some())
        }
        for (id, _) in to_flush.deleted {
            self.persistent.remove(&id);
        }
    }

    pub fn created(&self) -> &BTreeMap<ObjectID, Object> {
        &self.temporary.created
    }

    pub fn updated(&self) -> &BTreeMap<ObjectID, Object> {
        &self.temporary.updated
    }

    pub fn deleted(&self) -> &BTreeMap<ObjectID, (SequenceNumber, DeleteKind)> {
        &self.temporary.deleted
    }

    pub fn events(&self) -> &[Event] {
        &self.temporary.events
    }

    pub fn get_created_keys(&self) -> Vec<ObjectID> {
        self.temporary.created.keys().cloned().collect()
    }
}

impl Storage for InMemoryStorage {
    fn reset(&mut self) {
        self.temporary = ScratchPad::default();
    }

    fn read_object(&self, id: &ObjectID) -> Option<Object> {
        // there should be no read after delete
        assert!(!self.temporary.deleted.contains_key(id));
        // try objects updated in temp memory first
        self.temporary.updated.get(id).cloned().or_else(|| {
            self.temporary.created.get(id).cloned().or_else(||
                // try persistent memory
                 self.persistent.get(id).cloned())
        })
    }

    fn set_create_object_ids(&mut self, ids: HashSet<ObjectID>) {
        self.temporary.created_object_ids = ids;
    }

    // buffer write to appropriate place in temporary storage
    fn write_object(&mut self, object: Object) {
        let id = object.id();
        // there should be no write after delete
        assert!(!self.temporary.deleted.contains_key(&id));
        if self.persistent.contains_key(&id) {
            self.temporary.updated.insert(id, object);
        } else {
            self.temporary.created.insert(id, object);
        }
    }

    fn log_event(&mut self, event: Event) {
        self.temporary.events.push(event)
    }

    // buffer delete
    fn delete_object(&mut self, id: &ObjectID, version: SequenceNumber, kind: DeleteKind) {
        // there should be no deletion after write
        assert!(self.temporary.updated.get(id) == None);
        let old_entry = self.temporary.deleted.insert(*id, (version, kind));
        // this object was not previously deleted
        assert!(old_entry.is_none());
    }
}

impl ModuleResolver for InMemoryStorage {
    type Error = ();
    fn get_module(&self, module_id: &ModuleId) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self
            .read_object(&ObjectID::from(*module_id.address()))
            .map(|o| match &o.data {
                Data::Package(m) => m.serialized_module_map()[module_id.name().as_str()]
                    .clone()
                    .into_vec(),
                Data::Move(_) => panic!("Type error"),
            }))
    }
}

impl ResourceResolver for InMemoryStorage {
    type Error = ();

    fn get_resource(
        &self,
        _address: &AccountAddress,
        _struct_tag: &StructTag,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        unreachable!("Should never be called in Sui")
    }
}

#[allow(clippy::too_many_arguments)]
fn call(
    storage: &mut InMemoryStorage,
    native_functions: &NativeFunctionTable,
    module_name: &str,
    fun_name: &str,
    gas_budget: u64,
    type_args: Vec<TypeTag>,
    object_args: Vec<Object>,
    pure_args: Vec<Vec<u8>>,
) -> SuiResult<Vec<CallResult>> {
    let package = storage.find_package(module_name).unwrap();

    let vm = adapter::new_move_vm(native_functions.clone()).expect("No errors");
    adapter::execute(
        &vm,
        storage,
        native_functions,
        &package,
        &Identifier::new(module_name).unwrap(),
        &Identifier::new(fun_name).unwrap(),
        type_args,
        object_args,
        pure_args,
        &mut SuiGasStatus::new_with_budget(gas_budget, 1, 1),
        &mut TxContext::random_for_testing_only(),
    )
}

/// Exercise test functions that create, transfer, read, update, and delete objects
#[test]
fn test_object_basics() {
    let addr1 = base_types::get_new_address();
    let addr2 = base_types::get_new_address();

    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let mut storage = InMemoryStorage::new(genesis_objects);

    // 0. Create a gas object for gas payment.
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());
    storage.write_object(gas_object);
    storage.flush();

    // 1. Create obj1 owned by addr1
    // ObjectBasics::create expects integer value and recipient address
    let pure_args = vec![
        10u64.to_le_bytes().to_vec(),
        bcs::to_bytes(&AccountAddress::from(addr1)).unwrap(),
    ];
    call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "create",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        pure_args,
    )
    .unwrap();

    assert_eq!(storage.created().len(), 1);
    assert!(storage.updated().is_empty());
    assert!(storage.deleted().is_empty());
    let id1 = storage.get_created_keys().pop().unwrap();
    storage.flush();
    let mut obj1 = storage.read_object(&id1).unwrap();
    let mut obj1_seq = SequenceNumber::from(1);
    assert!(obj1.owner == addr1);
    assert_eq!(obj1.version(), obj1_seq);

    // 2. Transfer obj1 to addr2
    let pure_args = vec![bcs::to_bytes(&AccountAddress::from(addr2)).unwrap()];
    call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "transfer",
        GAS_BUDGET,
        Vec::new(),
        vec![obj1.clone()],
        pure_args,
    )
    .unwrap();

    assert_eq!(storage.updated().len(), 1);
    assert!(storage.created().is_empty());
    assert!(storage.deleted().is_empty());
    storage.flush();
    let transferred_obj = storage.read_object(&id1).unwrap();
    assert!(transferred_obj.owner == addr2);
    obj1_seq = obj1_seq.increment();
    assert_eq!(obj1.id(), transferred_obj.id());
    assert_eq!(transferred_obj.version(), obj1_seq);
    assert_eq!(
        obj1.data.try_as_move().unwrap().type_specific_contents(),
        transferred_obj
            .data
            .try_as_move()
            .unwrap()
            .type_specific_contents()
    );
    obj1 = transferred_obj;

    // 3. Create another object obj2 owned by addr2, use it to update addr1
    let pure_args = vec![
        20u64.to_le_bytes().to_vec(),
        bcs::to_bytes(&AccountAddress::from(addr2)).unwrap(),
    ];
    call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "create",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        pure_args,
    )
    .unwrap();
    let obj2 = storage
        .created()
        .values()
        .cloned()
        .collect::<Vec<Object>>()
        .pop()
        .unwrap();
    storage.flush();

    call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "update",
        GAS_BUDGET,
        Vec::new(),
        vec![obj1.clone(), obj2],
        Vec::new(),
    )
    .unwrap();
    assert_eq!(storage.updated().len(), 1);
    assert!(storage.created().is_empty());
    assert!(storage.deleted().is_empty());
    // test than an event was emitted as expected
    assert_eq!(storage.events().len(), 1);
    assert_eq!(
        storage.events()[0].clone().type_.name.to_string(),
        "NewValueEvent"
    );
    storage.flush();
    let updated_obj = storage.read_object(&id1).unwrap();
    assert!(updated_obj.owner == addr2);
    obj1_seq = obj1_seq.increment();
    assert_eq!(updated_obj.version(), obj1_seq);
    assert_ne!(
        obj1.data.try_as_move().unwrap().type_specific_contents(),
        updated_obj
            .data
            .try_as_move()
            .unwrap()
            .type_specific_contents()
    );
    obj1 = updated_obj;

    // 4. Delete obj1
    call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "delete",
        GAS_BUDGET,
        Vec::new(),
        vec![obj1],
        Vec::new(),
    )
    .unwrap();
    assert_eq!(storage.deleted().len(), 1);
    assert!(storage.created().is_empty());
    assert!(storage.updated().is_empty());
    storage.flush();
    assert!(storage.read_object(&id1).is_none())
}

/// Exercise test functions that wrap and object and subsequently unwrap it
/// Ensure that the object's version is consistent
#[test]
fn test_wrap_unwrap() {
    let addr = base_types::SuiAddress::default();

    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let mut storage = InMemoryStorage::new(genesis_objects);

    // 0. Create a gas object for gas payment. Note that we won't really use it because we won't be providing a gas budget.
    let gas_object = Object::with_id_owner_for_testing(ObjectID::random(), addr);
    storage.write_object(gas_object);
    storage.flush();

    // 1. Create obj1 owned by addr
    let pure_args = vec![
        10u64.to_le_bytes().to_vec(),
        bcs::to_bytes(&AccountAddress::from(addr)).unwrap(),
    ];
    call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "create",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        pure_args,
    )
    .unwrap();
    let id1 = storage.get_created_keys().pop().unwrap();
    storage.flush();
    let obj1 = storage.read_object(&id1).unwrap();
    let obj1_version = obj1.version();
    let obj1_contents = obj1
        .data
        .try_as_move()
        .unwrap()
        .type_specific_contents()
        .to_vec();
    assert_eq!(obj1.version(), SequenceNumber::from(1));

    // 2. wrap addr
    call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "wrap",
        GAS_BUDGET,
        Vec::new(),
        vec![obj1],
        Vec::new(),
    )
    .unwrap();
    // wrapping should create wrapper object and "delete" wrapped object
    assert_eq!(storage.created().len(), 1);
    assert_eq!(storage.deleted().len(), 1);
    assert_eq!(storage.deleted().iter().next().unwrap().0, &id1);
    let id2 = storage.get_created_keys().pop().unwrap();
    storage.flush();
    assert!(storage.read_object(&id1).is_none());
    let obj2 = storage.read_object(&id2).unwrap();

    // 3. unwrap addr
    call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "unwrap",
        GAS_BUDGET,
        Vec::new(),
        vec![obj2],
        Vec::new(),
    )
    .unwrap();
    // wrapping should delete wrapped object and "create" unwrapped object
    assert_eq!(storage.created().len(), 1);
    assert_eq!(storage.deleted().len(), 1);
    assert_eq!(storage.deleted().iter().next().unwrap().0, &id2);
    assert_eq!(id1, storage.get_created_keys().pop().unwrap());
    storage.flush();
    assert!(storage.read_object(&id2).is_none());
    let new_obj1 = storage.read_object(&id1).unwrap();
    // obj1 has gone through wrapping and unwrapping.
    // version number is now the original version + 2.
    assert_eq!(new_obj1.version(), obj1_version.increment().increment());
    // type-specific contents should not change after unwrapping
    assert_eq!(
        new_obj1
            .data
            .try_as_move()
            .unwrap()
            .type_specific_contents(),
        &obj1_contents
    );
}

#[test]
fn test_freeze() {
    let addr1 = base_types::get_new_address();

    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let mut storage = InMemoryStorage::new(genesis_objects);

    // 0. Create a gas object for gas payment.
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());
    storage.write_object(gas_object);
    storage.flush();

    // 1. Create obj1 owned by addr1
    // ObjectBasics::create expects integer value and recipient address
    let pure_args = vec![
        10u64.to_le_bytes().to_vec(),
        bcs::to_bytes(&AccountAddress::from(addr1)).unwrap(),
    ];
    call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "create",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        pure_args,
    )
    .unwrap();

    let id1 = storage.get_created_keys().pop().unwrap();
    storage.flush();
    let obj1 = storage.read_object(&id1).unwrap();
    assert!(!obj1.is_read_only());

    // 2. Call freeze_object.
    call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "freeze_object",
        GAS_BUDGET,
        Vec::new(),
        vec![obj1],
        vec![],
    )
    .unwrap();
    assert_eq!(storage.updated().len(), 1);
    storage.flush();
    let obj1 = storage.read_object(&id1).unwrap();
    assert!(obj1.is_read_only());
    assert!(obj1.owner == Owner::SharedImmutable);

    // 3. Call transfer again and it should fail.
    let pure_args = vec![bcs::to_bytes(&AccountAddress::from(addr1)).unwrap()];
    let result = call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "transfer",
        GAS_BUDGET,
        Vec::new(),
        vec![obj1],
        pure_args,
    );
    let err = result.unwrap_err();
    assert!(err
        .to_string()
        .contains("Shared object cannot be passed by-value, found in argument 0"));

    // 4. Call set_value (pass as mutable reference) should fail as well.
    let obj1 = storage.read_object(&id1).unwrap();
    let pure_args = vec![bcs::to_bytes(&1u64).unwrap()];
    let result = call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "set_value",
        GAS_BUDGET,
        Vec::new(),
        vec![obj1],
        pure_args,
    );
    let err = result.unwrap_err();
    assert!(err
        .to_string()
        .contains("Argument 0 is expected to be mutable, immutable object found"));
}

#[test]
fn test_move_call_args_type_mismatch() {
    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let mut storage = InMemoryStorage::new(genesis_objects);

    // 0. Create a gas object for gas payment.
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());
    storage.write_object(gas_object);
    storage.flush();

    // ObjectBasics::create expects 2 args: integer value and recipient address
    // Pass 1 arg only to trigger error.
    let pure_args = vec![10u64.to_le_bytes().to_vec()];
    let status = call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "create",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        pure_args,
    );
    let err = status.unwrap_err();
    assert!(err
        .to_string()
        .contains("Expected 3 arguments calling function 'create', but found 2"));

    /*
    // Need to fix https://github.com/MystenLabs/sui/issues/211
    // in order to enable the following test.
    let pure_args = vec![
        10u64.to_le_bytes().to_vec(),
        10u64.to_le_bytes().to_vec(),
    ];
    let status = call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "create",
        gas_object.clone(),
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        pure_args,
    )
    .unwrap();
    let (gas_used, err) = status.unwrap_err();
    assert_eq!(gas_used, gas::MIN_MOVE);
    // Assert on the error message as well.
    */
}

#[test]
fn test_move_call_incorrect_function() {
    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let mut storage = InMemoryStorage::new(genesis_objects);

    // 0. Create a gas object for gas payment.
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());
    storage.write_object(gas_object.clone());
    storage.flush();

    // Instead of calling on the genesis package, we are calling the gas object.
    let vm = adapter::new_move_vm(native_functions.clone()).expect("No errors");
    let status = adapter::execute(
        &vm,
        &mut storage,
        &native_functions,
        &gas_object,
        &Identifier::new("ObjectBasics").unwrap(),
        &Identifier::new("create").unwrap(),
        vec![],
        vec![],
        vec![],
        &mut SuiGasStatus::new_unmetered(),
        &mut TxContext::random_for_testing_only(),
    );
    let err = status.unwrap_err();
    assert!(err
        .to_string()
        .contains("Expected a module object, but found a Move object"));

    // Calling a non-existing function.
    let pure_args = vec![10u64.to_le_bytes().to_vec()];
    let status = call(
        &mut storage,
        &native_functions,
        "ObjectBasics",
        "foo",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        pure_args,
    );
    let err = status.unwrap_err();
    assert!(err.to_string().contains(&format!(
        "Could not resolve function 'foo' in module {}::ObjectBasics",
        SUI_FRAMEWORK_ADDRESS
    )));
}

#[test]
fn test_publish_module_linker_error() {
    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let id_module = CompiledModule::deserialize(
        genesis_objects[1]
            .data
            .try_as_package()
            .unwrap()
            .serialized_module_map()
            .get("ID")
            .unwrap(),
    )
    .unwrap();

    let mut storage = InMemoryStorage::new(genesis_objects);

    // 0. Create a gas object for gas payment.
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());
    storage.write_object(gas_object);
    storage.flush();

    // 1. Create a module that depends on a genesis module that exists, but via an invalid handle
    let mut dependent_module = file_format::empty_module();
    // make `dependent_module` depend on `id_module`
    dependent_module
        .identifiers
        .push(id_module.self_id().name().to_owned());
    dependent_module
        .address_identifiers
        .push(*id_module.self_id().address());
    dependent_module.module_handles.push(ModuleHandle {
        address: AddressIdentifierIndex((dependent_module.address_identifiers.len() - 1) as u16),
        name: IdentifierIndex((dependent_module.identifiers.len() - 1) as u16),
    });
    // now, the invalid part: add a StructHandle to `dependent_module` that doesn't exist in `m`
    dependent_module
        .identifiers
        .push(ident_str!("DoesNotExist").to_owned());
    dependent_module.struct_handles.push(StructHandle {
        module: ModuleHandleIndex((dependent_module.module_handles.len() - 1) as u16),
        name: IdentifierIndex((dependent_module.identifiers.len() - 1) as u16),
        abilities: AbilitySet::EMPTY,
        type_parameters: Vec::new(),
    });

    let mut module_bytes = Vec::new();
    dependent_module.serialize(&mut module_bytes).unwrap();
    let module_bytes = vec![module_bytes];

    let mut tx_context = TxContext::random_for_testing_only();
    let response = adapter::publish(
        &mut storage,
        native_functions,
        module_bytes,
        &mut tx_context,
        &mut SuiGasStatus::new_unmetered(),
    );
    let err = response.unwrap_err();
    let err_str = err.to_string();
    // make sure it's a linker error
    assert!(err_str.contains("VMError with status LOOKUP_FAILED"));
    // related to failed lookup of a struct handle
    assert!(err_str.contains("at index 0 for struct handle"))
}

#[test]
fn test_publish_module_non_zero_address() {
    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();

    let mut storage = InMemoryStorage::new(genesis_objects);

    // 0. Create a gas object for gas payment.
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());
    storage.write_object(gas_object);
    storage.flush();

    // 1. Create an empty module.
    let mut module = file_format::empty_module();
    // 2. Change the module address to non-zero.
    module.address_identifiers.pop();
    module.address_identifiers.push(AccountAddress::random());

    let mut module_bytes = Vec::new();
    module.serialize(&mut module_bytes).unwrap();
    let module_bytes = vec![module_bytes];

    let mut tx_context = TxContext::random_for_testing_only();
    let response = adapter::publish(
        &mut storage,
        native_functions,
        module_bytes,
        &mut tx_context,
        &mut SuiGasStatus::new_unmetered(),
    );
    let err = response.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("Publishing module")
            && err_str.contains("with non-zero address is not allowed")
    );
}

#[test]
fn test_coin_transfer() {
    let addr = base_types::SuiAddress::default();

    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();

    let mut storage = InMemoryStorage::new(genesis_objects);

    // 0. Create a gas object for gas payment. Note that we won't really use it because we won't be providing a gas budget.
    // 1. Create an object to transfer
    let gas_object = Object::with_id_owner_for_testing(ObjectID::random(), addr);
    let to_transfer = Object::with_id_owner_for_testing(ObjectID::random(), addr);
    storage.write_object(gas_object);
    storage.write_object(to_transfer.clone());
    storage.flush();

    let addr1 = sui_types::crypto::get_key_pair().0;

    call(
        &mut storage,
        &native_functions,
        "Coin",
        "transfer_",
        GAS_BUDGET,
        vec![GAS::type_tag()],
        vec![to_transfer],
        vec![
            10u64.to_le_bytes().to_vec(),
            bcs::to_bytes(&AccountAddress::from(addr1)).unwrap(),
        ],
    )
    .unwrap();

    // should update input coin
    assert_eq!(storage.updated().len(), 1);
    // should create one new coin
    assert_eq!(storage.created().len(), 1);
}

/// A helper function for publishing modules stored in source files.
fn publish_from_src(
    storage: &mut InMemoryStorage,
    natives: &NativeFunctionTable,
    src_path: &str,
    gas_object: Object,
    _gas_budget: u64,
) {
    storage.write_object(gas_object);
    storage.flush();

    // build modules to be published
    let build_config = BuildConfig::default();
    let mut module_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    module_path.push(src_path);
    let modules = sui_framework::build_move_package(&module_path, build_config, false).unwrap();

    // publish modules
    let all_module_bytes = modules
        .iter()
        .map(|m| {
            let mut module_bytes = Vec::new();
            m.serialize(&mut module_bytes).unwrap();
            module_bytes
        })
        .collect();
    let mut tx_context = TxContext::random_for_testing_only();
    adapter::publish(
        storage,
        natives.clone(),
        all_module_bytes,
        &mut tx_context,
        &mut SuiGasStatus::new_unmetered(),
    )
    .unwrap();
}

#[test]
fn test_simple_call() {
    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let mut storage = InMemoryStorage::new(genesis_objects);

    // crate gas object for payment
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());

    // publish modules at a given path
    publish_from_src(
        &mut storage,
        &native_functions,
        "src/unit_tests/data/simple_call",
        gas_object,
        GAS_BUDGET,
    );
    storage.flush();

    // call published module function
    let obj_val = 42u64;

    let addr = base_types::get_new_address();
    let pure_args = vec![
        obj_val.to_le_bytes().to_vec(),
        bcs::to_bytes(&AccountAddress::from(addr)).unwrap(),
    ];

    call(
        &mut storage,
        &native_functions,
        "M1",
        "create",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        pure_args,
    )
    .unwrap();

    // check if the object was created and if it has the right value
    let id = storage.get_created_keys().pop().unwrap();
    storage.flush();
    let obj = storage.read_object(&id).unwrap();
    assert!(obj.owner == addr);
    assert_eq!(obj.version(), SequenceNumber::from(1));
    let move_obj = obj.data.try_as_move().unwrap();
    assert_eq!(
        u64::from_le_bytes(move_obj.type_specific_contents().try_into().unwrap()),
        obj_val
    );
}

#[test]
/// Tests publishing of a module with a constructor that creates a
/// single object with a single u64 value 42.
fn test_publish_init() {
    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let mut storage = InMemoryStorage::new(genesis_objects);

    // crate gas object for payment
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());

    // publish modules at a given path
    publish_from_src(
        &mut storage,
        &native_functions,
        "src/unit_tests/data/publish_init",
        gas_object,
        GAS_BUDGET,
    );

    // a package object and a fresh object in the constructor should
    // have been crated
    assert_eq!(storage.created().len(), 2);
    let to_check = mem::take(&mut storage.temporary.created);
    let mut move_obj_exists = false;
    for o in to_check.values() {
        if let Data::Move(move_obj) = &o.data {
            move_obj_exists = true;
            assert_eq!(
                u64::from_le_bytes(move_obj.type_specific_contents().try_into().unwrap()),
                42u64
            );
        }
    }
    assert!(move_obj_exists);
}

#[test]
/// Tests public initializer that should not be executed upon
/// publishing the module.
fn test_publish_init_public() {
    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let mut storage = InMemoryStorage::new(genesis_objects);

    // crate gas object for payment
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());

    // publish modules at a given path
    publish_from_src(
        &mut storage,
        &native_functions,
        "src/unit_tests/data/publish_init_public",
        gas_object,
        GAS_BUDGET,
    );

    // only a package object should have been crated
    assert_eq!(storage.created().len(), 1);
}

#[test]
/// Tests initializer returning a value that should not be executed
/// upon publishing the module.
fn test_publish_init_ret() {
    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let mut storage = InMemoryStorage::new(genesis_objects);

    // crate gas object for payment
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());

    // publish modules at a given path
    publish_from_src(
        &mut storage,
        &native_functions,
        "src/unit_tests/data/publish_init_ret",
        gas_object,
        GAS_BUDGET,
    );

    // only a package object should have been crated
    assert_eq!(storage.created().len(), 1);
}

#[test]
/// Tests initializer with parameters other than &mut TxContext that
/// should not be executed upon publishing the module.
fn test_publish_init_param() {
    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let mut storage = InMemoryStorage::new(genesis_objects);

    // crate gas object for payment
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());

    // publish modules at a given path
    publish_from_src(
        &mut storage,
        &native_functions,
        "src/unit_tests/data/publish_init_param",
        gas_object,
        GAS_BUDGET,
    );

    // only a package object should have been crated
    assert_eq!(storage.created().len(), 1);
}

#[test]
/// Tests calls to entry functions returning values.
fn test_call_ret() {
    let native_functions =
        sui_framework::natives::all_natives(MOVE_STDLIB_ADDRESS, SUI_FRAMEWORK_ADDRESS);
    let genesis_objects = genesis::clone_genesis_packages();
    let mut storage = InMemoryStorage::new(genesis_objects);

    // crate gas object for payment
    let gas_object =
        Object::with_id_owner_for_testing(ObjectID::random(), base_types::SuiAddress::default());

    // publish modules at a given path
    publish_from_src(
        &mut storage,
        &native_functions,
        "src/unit_tests/data/call_ret",
        gas_object,
        GAS_BUDGET,
    );
    storage.flush();

    // call published module function returning a u64 (42)
    let response = call(
        &mut storage,
        &native_functions,
        "M1",
        "get_u64",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert!(matches!(response.get(0).unwrap(), CallResult::U64(42)));

    // call published module function returning an address (0x42)
    let response = call(
        &mut storage,
        &native_functions,
        "M1",
        "get_addr",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        response.get(0).unwrap(),
        &CallResult::Address(AccountAddress::from_hex_literal("0x42").unwrap()),
    );
    // call published module function returning two values: a u64 (42)
    // and an address (0x42)
    let response = call(
        &mut storage,
        &native_functions,
        "M1",
        "get_tuple",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert!(matches!(response.get(0).unwrap(), CallResult::U64(42),));
    assert_eq!(
        response.get(1).unwrap(),
        &CallResult::Address(AccountAddress::from_hex_literal("0x42").unwrap()),
    );

    // call published module function returning a vector
    let response = call(
        &mut storage,
        &native_functions,
        "M1",
        "get_vec",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(response.get(0).unwrap(), &CallResult::U64Vec(vec![42, 7]),);

    // call published module function returning a vector of vectors
    let response = call(
        &mut storage,
        &native_functions,
        "M1",
        "get_vec_vec",
        GAS_BUDGET,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        response.get(0).unwrap(),
        &CallResult::U64VecVec(vec![vec![42, 7]]),
    );
}
