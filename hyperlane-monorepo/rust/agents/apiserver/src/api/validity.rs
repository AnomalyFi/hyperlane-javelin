use std::ops::Deref;

use async_std::future::pending;
use ethers::abi::{token, Token};
use eyre::{Context, Report, Result};

use serde_json::json;
use tide::http::mime;
use tide::{Request, Response, Body};
use tracing::{debug, info, error};
use hyperlane_core::{Checkpoint, CheckpointWithMessageId, MultisigSignedCheckpoint, HyperlaneSignerExt};
use hyperlane_core::accumulator::merkle::Proof;
use crate::apiserver::{State, ValidityRequest, ValidityResponse};
use crate::msg::metadata::{MessageMetadataBuilder, MetadataBuilder, SubModuleMetadata};
use crate::msg::pending_message::PendingMessage;
use crate::msg::metadata::multisig::{MetadataToken, MultisigMetadata};

#[tracing::instrument(skip(req))]
pub async fn check_validity(mut req: Request<State>) -> tide::Result {
    info!("0");
    let ValidityRequest { domain, message } = req.body_json().await?;
    info!("1");

    let State {
        origin_chains,
        destination_chains,
        prover_syncs,
        dbs,
        whitelist,
        blacklist,
        msg_ctxs,
        signer,
        ..
    } = req.state().clone();

    // Get the prover from the prover_syncs map based on the domain.
    let prover = prover_syncs.get(&domain).cloned().ok_or_else(|| tide::Error::from_str(403, "Prover not found"))?;
    info!("2");

    // Get the database from the dbs map based on the domain.
    let db_clone = dbs.get(&domain).ok_or_else(|| tide::Error::from_str(402, "Database not found"))?.clone();
    info!("3");

    let destination_ctxs = destination_chains
        .keys()
        .filter(|&destination| destination != &domain)
        .map(|destination| {
            (
                destination.id(),
                msg_ctxs[&crate::apiserver::ContextKey {
                    origin: domain.id(),
                    destination: destination.id(),
                }]
                    .clone(),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    info!("destination ctxs: {:?}", destination_ctxs);

    let destination = message.destination;

    // Skip if not whitelisted.
    if !whitelist.msg_matches(&message, true) {
        debug!(?message, whitelist=?whitelist, "Message not whitelisted, skipping");

        let res = Response::builder(404).body(json!({
            "code": 404,
            "message": "not whitelisted"
        })).content_type(mime::JSON).build();
        return Ok(res);
    }
    info!("4");

    // Skip if the message is blacklisted.
    if blacklist.msg_matches(&message, false) {
        debug!(?message, blacklist=?blacklist, "Message blacklisted, skipping");

        let res = Response::builder(404).body(json!({
            "code": 404,
            "message": "is blacklisted"
        })).content_type(mime::JSON).build();
        return Ok(res);
    }
    info!("5");

    // Skip if the message is intended for this origin.
    if destination == db_clone.domain().id() {
        debug!(?message, "Message destined for self, skipping");

        let res = Response::builder(404).body(json!({
            "code": 404,
            "message": "destination is origin"
        })).content_type(mime::JSON).build();
        return Ok(res);
    }
    info!("6");


    debug!(%message, "Sending message to submitter");

    // MerkleTreeProcessor Logic
    prover
        .write()
        .await
        .ingest_message_id(message.id())
        .await
        .map_err(Report::from).expect("TODO: panic message");

    info!("7");

    let tree = prover.read().await.incremental;

    let origin_conf = origin_chains.get(db_clone.domain()).unwrap().clone();
    info!("8");
    info!("origin_conf: {:?}", origin_conf);

    let c = Checkpoint {
        root: tree.root(),
        index: tree.index(),
        merkle_tree_hook_address: origin_conf.addresses.merkle_tree_hook,
        mailbox_domain: domain.id(),
    };
    info!("ckp: {:?}", c);

    let cm = CheckpointWithMessageId {
        checkpoint: c,
        message_id: message.id(),
    };

    info!("ckp with mid: {:?}", cm);

    let signed_check = signer.sign(cm).await?;
    info!("9");
    info!("signed check: {:?}", signed_check);

    const CTX: &str = "When fetching message proof";
    let proof = prover
        .read()
        .await
        .get_proof(tree.index(), tree.index())
        .context(CTX)
        .map_err(Report::from).expect("TODO: panic message");
    info!("10");
    info!("proof: {:?}", proof);


    let mcm = MultisigSignedCheckpoint {
        checkpoint: cm,
        signatures: vec![signed_check.signature],
    };
    info!("mcm: {:?}", mcm);

    let meta1 = MultisigMetadata::new(
        mcm.clone(),
        tree.index(),
        Some(proof),
    );
    // let meta2 = MultisigMetadata::new(mcm, tree.index(), Some(proof));

    let meta1raw = format_metadata("merkle", meta1).unwrap();
    // let meta2raw = format_metadata("message_id", meta2).unwrap();
    let sub_meta1 = SubModuleMetadata::new(0, meta1raw);
    // let sub_meta2 = SubModuleMetadata::new(1, meta2raw);
    // Note: only one meta is needed as more of it will cause Arithemetic Overflow
    let metadata = format_aggregation_meta(&mut [sub_meta1, ], 1);
    

    // Finally, build the submit arg and dispatch it to the submitter.
    let pending_msg = PendingMessage::from_persisted_retries(
        message,
        destination_ctxs[&destination].clone(),
    );

    info!("{:?}", pending_msg);
    info!("destination mailbox: {:?}", pending_msg.ctx.destination_mailbox);

    // TODO: fix this
    let is_already_delivered = pending_msg.ctx
        .destination_mailbox
        .delivered(pending_msg.message.id())
        .await
        .context("checking message delivery status")
        .map_err(|err| {
            info!("err: {}", err);
        }).expect("TODO: panic message");

    info!("11");

    if is_already_delivered {
        debug!("Message has already been delivered, marking as submitted.");
        let res = Response::builder(404).body(json!({
            "code": 404,
            "message": "message already delivered"
        })).content_type(mime::JSON).build();
        return Ok(res)
    }

    info!("12");

    let provider = pending_msg.ctx.destination_mailbox.provider();
    info!("{:?}", provider);
    info!("12.1");

    // We cannot deliver to an address that is not a contract so check and drop if it isn't.
    let is_contract = provider.is_contract(&pending_msg.message.recipient).await.context("checking if message recipient is a contract").map_err(Report::from).expect("TODO: panic message");
    info!("13");

    if !is_contract {
        info!(
            recipient=?pending_msg.message.recipient,
            "Dropping message because recipient is not a contract"
        );
        let res = Response::builder(404).body(json!({
            "code": 404,
            "message": "recipient is not a contract"
        })).content_type(mime::JSON).build();
        return Ok(res);
    }
    info!("14");

    let tx_cost_estimate = pending_msg.ctx
        .destination_mailbox
        .process_estimate_costs(&pending_msg.message, &metadata)
        .await
        .context("estimating costs for process call")
        .map_err(|r| {
            let err = r.source();
            info!("error process estimate costs: {:?}", err);
        }).expect("TODO: panic message");
    info!("16");

    let Some(gas_limit) = pending_msg.ctx
        .origin_gas_payment_enforcer
        .message_meets_gas_payment_requirement(&pending_msg.message, &tx_cost_estimate)
        .await
        .context("checking if message meets gas payment requirement").map_err(Report::from).expect("TODO: panic message") else {
        info!(?tx_cost_estimate, "Gas payment requirement not met yet");
        let res = Response::builder(404).body(json!({
            "code": 404,
            "message": "gas payment not met yet"
        })).content_type(mime::JSON).build();
        return Ok(res);
    };
    info!("17");

    debug!(
        ?gas_limit,
        "Gas payment requirement met, ready to process message"
    );

    let gas_limit = tx_cost_estimate.gas_limit;

    if let Some(max_limit) = pending_msg.ctx.transaction_gas_limit {
        if gas_limit > max_limit {
            info!("Message delivery estimated gas exceeds max gas limit");
            let res = Response::builder(404).body(json!({
                "code": 404,
                "message": "delivery gas exceeds estimation"
            })).content_type(mime::JSON).build();
            return Ok(res);
        }
    }
    info!("18");


    let response_body = ValidityResponse {
        message: pending_msg.message,
        metadata: metadata,
        gas_limit,
    };
    info!("19");

    let mut res = Response::new(200);
    res.set_body(Body::from_json(&response_body)?);
    Ok(res)
}

const METADATA_RANGE_SIZE: usize = 4;

fn format_aggregation_meta(metadatas: &mut [SubModuleMetadata], ism_count: usize) -> Vec<u8> {
    // See test solidity implementation of this fn at:
    // https://github.com/hyperlane-xyz/hyperlane-monorepo/blob/445da4fb0d8140a08c4b314e3051b7a934b0f968/solidity/test/isms/AggregationIsm.t.sol#L35
    fn encode_byte_index(i: usize) -> [u8; 4] {
        (i as u32).to_be_bytes()
    }
    let range_tuples_size = METADATA_RANGE_SIZE * 2 * ism_count;
    //  Format of metadata:
    //  [????:????] Metadata start/end uint32 ranges, packed as uint64
    //  [????:????] ISM metadata, packed encoding
    // Initialize the range tuple part of the buffer, so the actual metadatas can
    // simply be appended to it
    let mut buffer = vec![0; range_tuples_size];
    for SubModuleMetadata { index, metadata } in metadatas.iter_mut() {
        let range_start = buffer.len();
        buffer.append(metadata);
        let range_end = buffer.len();

        // The new tuple starts at the end of the previous ones.
        // Also see: https://github.com/hyperlane-xyz/hyperlane-monorepo/blob/445da4fb0d8140a08c4b314e3051b7a934b0f968/solidity/contracts/libs/isms/AggregationIsmMetadata.sol#L49
        let encoded_range_start = METADATA_RANGE_SIZE * 2 * (*index);
        // Overwrite the 0-initialized buffer
        buffer.splice(
            encoded_range_start..(encoded_range_start + METADATA_RANGE_SIZE * 2),
            [encode_byte_index(range_start), encode_byte_index(range_end)].concat(),
        );
    }
    buffer
}

fn format_metadata(t: &str, metadata: MultisigMetadata) -> Result<Vec<u8>> {
    let merkle_token_layout = vec![
        MetadataToken::CheckpointMerkleTreeHook,
        MetadataToken::MessageMerkleLeafIndex,
        MetadataToken::MessageId,
        MetadataToken::MerkleProof,
        MetadataToken::CheckpointIndex,
        MetadataToken::Signatures,
    ];
    let message_id_token_layout = vec![
        MetadataToken::CheckpointMerkleTreeHook,
        MetadataToken::CheckpointMerkleRoot,
        MetadataToken::CheckpointIndex,
        MetadataToken::Signatures,
    ];

    let token_layout:  &Vec<MetadataToken>;
    if t == "merkle" {
        token_layout = &merkle_token_layout;
    } else if t == "message_id" {
        token_layout = &message_id_token_layout; 
    } else {
        return Err(Report::msg("no supported token layout"))
    }

    let build_token = |token: &MetadataToken| -> Result<Vec<u8>> {
        match token {
            MetadataToken::CheckpointMerkleRoot => {
                Ok(metadata.checkpoint.root.to_fixed_bytes().into())
            }
            MetadataToken::MessageMerkleLeafIndex => {
                Ok(metadata.merkle_leaf_index.to_be_bytes().into())
            }
            MetadataToken::CheckpointIndex => {
                Ok(metadata.checkpoint.index.to_be_bytes().into())
            }
            MetadataToken::CheckpointMerkleTreeHook => Ok(metadata
                .checkpoint
                .merkle_tree_hook_address
                .to_fixed_bytes()
                .into()),
            MetadataToken::MessageId => {
                Ok(metadata.checkpoint.message_id.to_fixed_bytes().into())
            }
            MetadataToken::MerkleProof => {
                let proof_tokens: Vec<Token> = metadata
                    .proof
                    .unwrap()
                    .path
                    .iter()
                    .map(|x| Token::FixedBytes(x.to_fixed_bytes().into()))
                    .collect();
                Ok(ethers::abi::encode(&proof_tokens))
            }
            MetadataToken::Signatures => Ok(metadata
                .signatures
                .iter()
                .map(|x| x.to_vec())
                .collect::<Vec<_>>()
                .concat()),
        }
    };
    let metas: Result<Vec<Vec<u8>>> = token_layout.deref().iter().map(build_token).collect();
    Ok(metas?.into_iter().flatten().collect())
}