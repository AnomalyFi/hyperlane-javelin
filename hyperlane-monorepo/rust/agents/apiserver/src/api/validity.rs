use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use ethers::abi::{Token};
use eyre::{Context, Report, Result};

use tide::{Request, Response, Body};
use tokio::sync::RwLock;
use tracing::{debug, info};
use hyperlane_base::db::HyperlaneRocksDB;
use hyperlane_base::settings::ChainConf;
use hyperlane_core::{Checkpoint, CheckpointWithMessageId, MultisigSignedCheckpoint, HyperlaneSignerExt, HyperlaneDomain};
use hyperlane_ethereum::SingletonSignerHandle;

use crate::api::api_msgs::{new_validity_response_error, ValidityBatchRequest, ValidityBatchResponse, ValidityRequest, ValidityResponse};
use crate::apiserver::{ContextKey, State};
use crate::merkle_tree::builder::MerkleTreeBuilder;
use crate::msg::pending_message::{MessageContext, PendingMessage};
use crate::msg::metadata::multisig::{MetadataToken, MultisigMetadata};
use crate::msg::metadata::SubModuleMetadata;
use crate::settings::matching_list::MatchingList;

#[tracing::instrument(skip(req))]
pub async fn check_validity_request(mut req: Request<State>) -> tide::Result {
    info!("0");
    let validity_request: ValidityRequest = req.body_json().await?;
    info!("1");

    // We clone the state here so now reason to redo the clone within the function itself
    let state = req.state().clone();

    match check_validity(&validity_request, &state).await {
        Ok(validity_resp) => {
            let mut res = Response::new(200);
            res.set_body(Body::from_json(&validity_resp)?);
            Ok(res)
        }
        Err(e) => {
            let mut res = Response::new(404);
            let response_body = new_validity_response_error(e);
            res.set_body(Body::from_json(&response_body)?);
            Ok(res)
        }
    }
}

#[tracing::instrument(skip(validity_request))]
pub async fn check_validity(validity_request: &ValidityRequest, state: &State) -> Result<ValidityResponse, String> {
    let ValidityRequest {
        domain,
        message,
    } = validity_request;

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
    } = state;

    // Get the prover from the prover_syncs map based on the domain.
    let prover = prover_syncs.get(&domain).ok_or_else(|| "Prover not found")?;
    info!("2");

    // Get the database from the dbs map based on the domain.
    let db_clone = dbs.get(&domain)
        .ok_or_else(|| "Database not found")?;

    info!("3");

    let destination_ctxs = destination_chains
        .keys()
        .filter(|&destination| destination != domain)
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
        let err_msg = "Message not whitelisted, skipping";
        debug!(?message, whitelist=?whitelist, err_msg);
        return Err(err_msg.to_string());
    }
    info!("4");

    // Skip if the message is blacklisted.
    if blacklist.msg_matches(&message, false) {
        let err_msg = "Message blacklisted, skipping";
        debug!(?message, blacklist=?blacklist, err_msg);
        return Err(err_msg.to_string());
    }
    info!("5");

    // Skip if the message is intended for this origin.
    if destination == db_clone.domain().id() {
        let err_msg = "Message destined for self, skipping";
        debug!(?message, err_msg);
        return Err(err_msg.to_string());
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

    let signed_check = signer.sign(cm).await.or_else(|err| Err(err.to_string()))?;
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
    
    // Finally, build submit arg and dispatch it to the submitter.
    let pending_msg = PendingMessage::from_persisted_retries(
        message.clone(),
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
        let err_msg = "Message has already been delivered, marking as submitted.";
        debug!(err_msg);
        return Err(err_msg.to_string());
    }

    info!("12");

    let provider = pending_msg.ctx.destination_mailbox.provider();
    info!("{:?}", provider);
    info!("12.1");

    // We cannot deliver to an address that is not a contract so check and drop if it isn't.
    let is_contract = provider.is_contract(&pending_msg.message.recipient).await.context("checking if message recipient is a contract").map_err(Report::from).expect("TODO: panic message");
    info!("13");

    if !is_contract {
        let err_msg = "Dropping message because recipient is not a contract";
        info!(
            recipient=?pending_msg.message.recipient,
            err_msg
        );
        return Err(err_msg.to_string());
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
        .context("checking if message meets gas payment requirement")
        .map_err(Report::from)
        .expect("TODO: panic message")
        else {
            let err_msg = "Gas payment requirement not met yet";
            info!(?tx_cost_estimate, err_msg);
            return Err(err_msg.to_string());
        };

    info!("17");

    debug!(
        ?gas_limit,
        "Gas payment requirement met, ready to process message"
    );

    let gas_limit = tx_cost_estimate.gas_limit;

    if let Some(max_limit) = pending_msg.ctx.transaction_gas_limit {
        if gas_limit > max_limit {
            let err_msg = "Message delivery estimated gas exceeds max gas limit";
            info!(err_msg);
            return Err(err_msg.to_string());
        }
    }
    info!("18");

    let response_body = ValidityResponse {
        message: pending_msg.message,
        metadata: metadata,
        gas_limit,
        error: None,
    };
    info!("19");

    Ok(response_body)
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

#[derive(Clone, Debug)]
pub struct DBState {
    pub origin_chains: HashMap<HyperlaneDomain, ChainConf>,
    pub destination_chains: HashMap<HyperlaneDomain, ChainConf>,
    pub prover_syncs: HashMap<HyperlaneDomain, Arc<RwLock<MerkleTreeBuilder>>>,
    pub dbs: HashMap<HyperlaneDomain, HyperlaneRocksDB>,
    pub whitelist: Arc<MatchingList>,
    pub blacklist: Arc<MatchingList>,
    pub msg_ctxs: HashMap<ContextKey, Arc<MessageContext>>,
    pub signer: SingletonSignerHandle,
}

pub async fn batch_check_validity_request(mut req: Request<State>) -> tide::Result {
    let validity_batch_request : ValidityBatchRequest = req.body_json().await?;
    let state = req.state().clone();


    //The reason for this setup is because Tide State is kept common across requests so its useful for stuff like DB connections
    // In our case we want to clone the state once at the beginning of a batch of requests so we can work off of that state for an entire block
    // This allows the API Server to keep its sync element from when we forked the relayer code but also allows for common state within a block
    // https://docs.rs/tide/latest/tide/struct.Server.html

    let batch_response = batch_check_validity(&validity_batch_request, &state).await;

    let mut res = Response::new(200);
    res.set_body(Body::from_json(&batch_response)?);
    Ok(res)
}

pub async fn batch_check_validity(validity_batch_request: &ValidityBatchRequest, state: &State) -> ValidityBatchResponse {
    let mut response_body = ValidityBatchResponse::new();

    for request in &validity_batch_request.requests {
        let validity_response = check_validity(&request, &state).await.unwrap_or_else(|e| new_validity_response_error(e));

        response_body.responses.push(validity_response)
    }

    response_body
}

#[cfg(test)]
mod test {
    use hyperlane_base::db::test_utils;
    use hyperlane_core::HyperlaneMessage;
    use crate::api::api_msgs::{ValidityBatchRequest, ValidityRequest};
    use crate::api::validity::{batch_check_validity, check_validity};
    use crate::testutils::TestFixture;

    #[tokio::test]
    async fn test_basic_validity() {
        test_utils::run_test_db(|_db| async move {
            let test_fixture = TestFixture::new();

            let mut test_message: HyperlaneMessage = Default::default();
            test_message.origin = test_fixture.get_from_domain_id();
            test_message.destination = test_fixture.get_to_domain_id();

            let request = ValidityRequest {
                domain: test_fixture.get_from_domain(),
                message: test_message,
            };

            match check_validity(&request, test_fixture.get_state()).await {
                Ok(_) => {},
                Err(err_msg) => {
                    println!("Error occurred: {}", err_msg);
                    assert!(false);
                }
            }
        })
        .await;
    }

    #[tokio::test]
    async fn test_batch_validity() {
        test_utils::run_test_db(|_db| async move {
            let test_fixture = TestFixture::new();

            let mut validity_batch_request = ValidityBatchRequest::new();

            let num_test_msgs = 3;

            for _i in 0..num_test_msgs {
                let mut test_message: HyperlaneMessage = Default::default();
                test_message.origin = test_fixture.get_from_domain_id();
                test_message.destination = test_fixture.get_to_domain_id();

                let request = ValidityRequest {
                    domain: test_fixture.get_from_domain(),
                    message: test_message,
                };

                validity_batch_request.requests.push(request);
            }

            let result = batch_check_validity(&validity_batch_request, test_fixture.get_state()).await;

            assert_eq!(num_test_msgs, result.responses.len());
            assert!(result.is_ok())
        })
            .await;
    }
}