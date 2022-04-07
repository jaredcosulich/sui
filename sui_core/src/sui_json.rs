// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use anyhow::anyhow;
use move_core_types::{account_address::AccountAddress, identifier::Identifier};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use sui_types::{
    base_types::{decode_bytes_hex, ObjectID, SuiAddress},
    move_package::is_primitive,
    object::Object,
};

// Alias the type names for clarity
use move_binary_format::normalized::{Function as MoveFunction, Type as NormalizedMoveType};
use serde_json::Value as JsonValue;

const HEX_PREFIX: &str = "0x";

#[cfg(test)]
#[path = "unit_tests/sui_json.rs"]
mod base_types_tests;

#[derive(Eq, PartialEq, Debug, Clone, Deserialize, Serialize)]
pub enum IntermediateValue {
    Bool(bool),
    U8(u8),
    U64(u64),
    U128(u128),
    Address(SuiAddress),
    ObjectID(ObjectID),
    Vector(Vec<IntermediateValue>),
}

#[derive(Eq, PartialEq, Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SuiJsonValue(JsonValue);
impl SuiJsonValue {
    pub fn new(json_value: JsonValue) -> Result<SuiJsonValue, anyhow::Error> {
        match json_value.clone() {
            // No checks needed for Bool and String
            JsonValue::Bool(_) | JsonValue::String(_) => (),
            JsonValue::Number(n) => {
                // Must be castable to u64
                if !n.is_u64() {
                    return Err(anyhow!("{n} not allowed. Number must be unsigned integer"));
                }
            }
            // Must be homogeneous
            JsonValue::Array(a) => {
                // Fail if not homogeneous
                if !is_homogenous(&JsonValue::Array(a)) {
                    return Err(anyhow!("Arrays must be homogeneous",));
                }
            }
            _ => return Err(anyhow!("{json_value} not allowed.")),
        };
        Ok(Self(json_value))
    }

    pub fn to_bcs_bytes(&self, typ: &NormalizedMoveType) -> Result<Vec<u8>, anyhow::Error> {
        let intermediate_val = Self::to_intermediate_value(&self.0, typ)?;

        fn inner_serialize(
            inter_val: IntermediateValue,
            ty: &NormalizedMoveType,
        ) -> Result<Vec<u8>, anyhow::Error> {
            let ser = match (inter_val.clone(), ty) {
                (IntermediateValue::Bool(b), NormalizedMoveType::Bool) => bcs::to_bytes(&b)?,
                (IntermediateValue::U8(n), NormalizedMoveType::U8) => bcs::to_bytes(&n)?,
                (IntermediateValue::U64(n), NormalizedMoveType::U64) => bcs::to_bytes(&n)?,
                (IntermediateValue::U128(n), NormalizedMoveType::U128) => bcs::to_bytes(&n)?,

                (IntermediateValue::Address(a), NormalizedMoveType::Address) => {
                    bcs::to_bytes(&AccountAddress::from(a))?
                }

                // Not currently used
                // (IntermediateValue::ObjectID(a), NormalizedMoveType::Address) => {
                //     bcs::to_bytes(&AccountAddress::from(a))?
                // }
                (IntermediateValue::Vector(v), NormalizedMoveType::Vector(move_type)) => {
                    let mut inner_ser = vec![];
                    let arr_len = v.len();
                    for i in v {
                        // Serialize each
                        inner_ser.append(&mut inner_serialize(i, move_type)?);
                    }
                    // The data is already serialized, so ideally we just append
                    // First serialize the types like they u8s
                    // We use this to create the ULEB128 length prefix
                    let u8vec = vec![0u8; arr_len];
                    let mut ser_container = bcs::to_bytes::<Vec<u8>>(&u8vec)?;
                    // Delete the zeroes
                    ser_container.truncate(ser_container.len() - arr_len);
                    // Append the actual data data
                    ser_container.append(&mut inner_ser);

                    ser_container
                }
                _ => {
                    return Err(anyhow!(
                        "Unable to serialize {:?}. Expected {}",
                        inter_val,
                        ty
                    ))
                }
            };
            Ok(ser)
        }
        inner_serialize(intermediate_val, typ)
    }

    pub fn to_json_value(&self) -> JsonValue {
        self.0.clone()
    }

    fn to_intermediate_value(
        val: &JsonValue,
        typ: &NormalizedMoveType,
    ) -> Result<IntermediateValue, anyhow::Error> {
        let new_serde_value = match (val, typ.clone()) {
            // Bool to Bool is simple
            (JsonValue::Bool(b), NormalizedMoveType::Bool) => IntermediateValue::Bool(*b),

            // In constructor, we have already checked that the JSON number is unsigned int of at most U64
            // Hence it is okay to unwrap() numbers
            (JsonValue::Number(n), NormalizedMoveType::U8) => {
                IntermediateValue::U8(u8::try_from(n.as_u64().unwrap())?)
            }
            (JsonValue::Number(n), NormalizedMoveType::U64) => {
                IntermediateValue::U64(n.as_u64().unwrap())
            }

            // u8, u64, u128 can be encoded as String
            (JsonValue::String(s), NormalizedMoveType::U8) => {
                IntermediateValue::U8(u8::try_from(convert_string_to_u128(s.as_str())?)?)
            }
            (JsonValue::String(s), NormalizedMoveType::U64) => {
                IntermediateValue::U64(u64::try_from(convert_string_to_u128(s.as_str())?)?)
            }
            (JsonValue::String(s), NormalizedMoveType::U128) => {
                IntermediateValue::U128(convert_string_to_u128(s.as_str())?)
            }

            // U256 Not allowed for now

            // We can encode U8 Vector as string in 2 ways
            // 1. If it starts with 0x, we treat it as hex strings, where each pair is a byte
            // 2. If it does not start with 0x, we treat each character as an ASCII encoded byte
            // We have to support both for the convenience of the user. This is because sometime we need Strings as arg
            // Other times we need vec of hex bytes for address. Issue is both Address and Strings are represented as Vec<u8> in Move call
            (JsonValue::String(s), NormalizedMoveType::Vector(t)) => {
                if *t != NormalizedMoveType::U8 {
                    return Err(anyhow!("Cannot convert string arg {s} to {typ}"));
                }
                let vec = if s.starts_with(HEX_PREFIX) {
                    // If starts with 0x, treat as hex vector
                    hex::decode(s.trim_start_matches(HEX_PREFIX))?
                } else {
                    // Else raw bytes
                    s.as_bytes().to_vec()
                };
                IntermediateValue::Vector(vec.iter().map(|q| IntermediateValue::U8(*q)).collect())
            }

            // We have already checked that the array is homogeneous in the constructor
            (JsonValue::Array(a), NormalizedMoveType::Vector(t)) => {
                // Recursively build an IntermediateValue array
                IntermediateValue::Vector(
                    a.iter()
                        .map(|i| Self::to_intermediate_value(i, &t))
                        .collect::<Result<Vec<IntermediateValue>, _>>()?,
                )
            }

            (JsonValue::String(s), NormalizedMoveType::Address) => {
                let s = s.trim().to_lowercase();
                if !s.starts_with(HEX_PREFIX) {
                    return Err(anyhow!("Address hex string must start with 0x.",));
                }
                let r: SuiAddress = decode_bytes_hex(s.trim_start_matches(HEX_PREFIX))?;
                IntermediateValue::Address(r)
            }
            _ => return Err(anyhow!("Unexpected arg {val} for expected type {typ}")),
        };

        Ok(new_serde_value)
    }
}

impl std::str::FromStr for SuiJsonValue {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, anyhow::Error> {
        SuiJsonValue::new(serde_json::from_str(s)?)
    }
}

#[derive(Eq, PartialEq, Debug, Clone, Hash)]
enum ValidJsonType {
    Bool,
    Number,
    String,
    Array,
    // Matches any type
    Any,
}

/// Check via BFS
/// The invariant is that all types at a given level must be the same or be empty
pub fn is_homogenous(val: &JsonValue) -> bool {
    let mut deq: VecDeque<&JsonValue> = VecDeque::new();
    deq.push_back(val);
    is_homogenous_rec(&mut deq)
}

/// Check via BFS
/// The invariant is that all types at a given level must be the same or be empty
fn is_homogenous_rec(curr_q: &mut VecDeque<&JsonValue>) -> bool {
    if curr_q.is_empty() {
        // Nothing to do
        return true;
    }
    // Queue for the next level
    let mut next_q = VecDeque::new();
    // The types at this level must be the same
    let mut level_type = ValidJsonType::Any;

    // Process all in this queue/level
    while !curr_q.is_empty() {
        // Okay to unwrap since we know values exist
        let curr = match curr_q.pop_front().unwrap() {
            JsonValue::Bool(_) => ValidJsonType::Bool,
            JsonValue::Number(_) => ValidJsonType::Number,
            JsonValue::String(_) => ValidJsonType::String,
            JsonValue::Array(w) => {
                // Add to the next level
                w.iter().for_each(|t| next_q.push_back(t));
                ValidJsonType::Array
            }
            // Not valid
            _ => return false,
        };

        if level_type == ValidJsonType::Any {
            // Update the level with the first found type
            level_type = curr;
        } else if level_type != curr {
            // Mismatch in the level
            return false;
        }
    }
    // Process the next level
    is_homogenous_rec(&mut next_q)
}

fn check_and_serialize_pure_args(
    args: &[SuiJsonValue],
    start: usize,
    end_exclusive: usize,
    function_signature: MoveFunction,
) -> Result<Vec<Vec<u8>>, anyhow::Error> {
    // The vector of serialized arguments
    let mut pure_args_serialized = vec![];

    // Iterate through the pure args
    for (idx, curr) in args
        .iter()
        .enumerate()
        .skip(start)
        .take(end_exclusive - start)
    {
        // The type the function expects at this position
        let expected_pure_arg_type = &function_signature.parameters[idx];

        // Check that the args are what we expect or can be converted
        // Then return the serialized bcs value
        match curr.to_bcs_bytes(expected_pure_arg_type) {
            Ok(a) => {
                pure_args_serialized.push(a.clone());
            }
            Err(e) => return Err(anyhow!("Unable to parse arg at pos: {}, err: {:?}", idx, e)),
        }
    }
    Ok(pure_args_serialized)
}

fn resolve_object_args(
    args: &[SuiJsonValue],
    start: usize,
    end_exclusive: usize,
) -> Result<Vec<ObjectID>, anyhow::Error> {
    // Every elem has to be a string convertible to a ObjectID
    let mut object_args_ids = vec![];
    for (idx, arg) in args
        .iter()
        .enumerate()
        .take(end_exclusive - start)
        .skip(start)
    {
        let transformed = match arg.to_json_value() {
            JsonValue::String(s) => {
                let  s = s.trim().to_lowercase();
                if !s.starts_with(HEX_PREFIX) {
                    return Err(anyhow!(
                        "ObjectID hex string must start with 0x.",
                    ))
                }
                ObjectID::from_hex_literal(&s)?
            }
            _ => {
                return Err(anyhow!(
                    "Unable to parse arg {:?} as ObjectID at pos {}. Expected {:?} byte hex string prefixed with 0x.",
                    ObjectID::LENGTH,
                    idx,
                    arg.to_json_value(),
                ))
            }
        };

        object_args_ids.push(transformed);
    }
    Ok(object_args_ids)
}

/// Resolve a the JSON args of a function into the expected formats to make them usable by Move call
/// This is because we have special types which we need to specify in other formats
pub fn resolve_move_function_args(
    package: &Object,
    module: Identifier,
    function: Identifier,
    combined_args_json: Vec<SuiJsonValue>,
) -> Result<(Vec<ObjectID>, Vec<Vec<u8>>), anyhow::Error> {
    // Extract the expected function signature
    let function_signature = package
        .data
        .try_as_package()
        .ok_or_else(|| anyhow!("Cannot get package from object"))?
        .get_function_signature(&module, &function)?;

    // Lengths have to match, less one, due to TxContext
    let expected_len = function_signature.parameters.len() - 1;
    if combined_args_json.len() != expected_len {
        return Err(anyhow!(
            "Expected {} args, found {}",
            expected_len,
            combined_args_json.len()
        ));
    }

    // Object args must always precede the pure/primitive args, so extract those first
    // Find the first non-object args, which marks the start of the pure args
    // Find the first pure/primitive type
    let pure_args_start = function_signature
        .parameters
        .iter()
        .position(is_primitive)
        .unwrap_or(expected_len);

    // Everything to the left of pure args must be object args

    // Check that the object args are valid
    let obj_args = resolve_object_args(&combined_args_json, 0, pure_args_start)?;

    // Check that the pure args are valid or can be made valid
    let pure_args_serialized = check_and_serialize_pure_args(
        &combined_args_json,
        pure_args_start,
        expected_len,
        function_signature,
    )?;

    Ok((obj_args, pure_args_serialized))
}

fn convert_string_to_u128(s: &str) -> Result<u128, anyhow::Error> {
    // Try as normal number
    if let Ok(v) = s.parse::<u128>() {
        return Ok(v);
    }

    // Check prefix
    // For now only Hex supported
    // TODO: add support for bin and octal?

    let s = s.trim().to_lowercase();
    if !s.starts_with(HEX_PREFIX) {
        return Err(anyhow!("Unable to convert {s} to unsigned int.",));
    }
    u128::from_str_radix(s.trim_start_matches(HEX_PREFIX), 16).map_err(|e| e.into())
}
