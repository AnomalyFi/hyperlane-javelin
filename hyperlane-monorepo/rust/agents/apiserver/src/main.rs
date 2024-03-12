//TODO change this
//! At a regular interval, the APIServer polls the current chain's mailbox for
//! signed checkpoints and submits them as checkpoints on the remote mailbox.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use eyre::Result;

use hyperlane_base::agent_main;

use crate::apiserver::APIServer;

mod merkle_tree;
mod msg;
mod processor;
mod prover;
mod apiserver;
mod settings;
mod api;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    agent_main::<APIServer>().await
}
