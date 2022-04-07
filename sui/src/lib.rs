// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

extern crate core;

use std::path::PathBuf;

use anyhow::bail;

pub mod config;
pub mod gateway;
pub mod keystore;
pub mod rest_gateway;
pub mod shell;
pub mod sui_commands;
pub mod wallet_commands;

const SUI_DIR: &str = ".sui";
const SUI_CONFIG_DIR: &str = "sui_config";
pub const SUI_NETWORK_CONFIG: &str = "network.conf";
pub const SUI_WALLET_CONFIG: &str = "wallet.conf";
pub const SUI_GATEWAY_CONFIG: &str = "gateway.conf";

pub fn sui_config_dir() -> Result<PathBuf, anyhow::Error> {
    match dirs::home_dir() {
        Some(v) => Ok(v.join(SUI_DIR).join(SUI_CONFIG_DIR)),
        None => bail!("Cannot obtain home directory path"),
    }
}
