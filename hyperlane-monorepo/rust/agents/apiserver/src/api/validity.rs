use ethers::abi::Token;
use eyre::{Context, Report, Result};

use tide::{Request, Response, Body};
use tracing::{debug, info, error};
use hyperlane_core::{Checkpoint, CheckpointWithMessageId, MultisigSignedCheckpoint, HyperlaneSignerExt, HyperlaneDomain, HyperlaneMessage};
use hyperlane_core::accumulator::merkle::Proof;
use crate::api::api_msgs::{new_validity_response_error, ValidityBatchRequest, ValidityBatchResponse, ValidityRequest, ValidityResponse};
use crate::apiserver::State;
use crate::msg::pending_message::PendingMessage;
use crate::msg::metadata::multisig::{MetadataToken, MultisigMetadata};

pub async fn check_validity_request(mut req: Request<State>) -> tide::Result {
    let validity_request: ValidityRequest = req.body_json().await?;
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
    let prover = prover_syncs.get(&domain).cloned().ok_or_else(|| "Prover not found")?;

    // Get the database from the dbs map based on the domain.
    let db_clone = dbs.get(&domain)
        .ok_or_else(|| "Database not found")?
        .clone();

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

    let destination = message.destination;

    // Skip if not whitelisted.
    if !whitelist.msg_matches(&message, true) {
        let err_msg = "Message not whitelisted, skipping";
        debug!(?message, whitelist=?whitelist, err_msg);
        return Err(err_msg.to_string());
    }

    // Skip if the message is blacklisted.
    if blacklist.msg_matches(&message, false) {
        let err_msg = "Message blacklisted, skipping";
        debug!(?message, blacklist=?blacklist, err_msg);
        return Err(err_msg.to_string());
    }

    // Skip if the message is intended for this origin.
    if destination == db_clone.domain().id() {
        let err_msg = "Message destined for self, skipping";
        debug!(?message, err_msg);
        return Err(err_msg.to_string());
    }

    debug!(%message, "Sending message to submitter");

    // MerkleTreeProcessor Logic
    prover
        .write()
        .await
        .ingest_message_id(message.id())
        .await
        .map_err(Report::from).expect("TODO: panic message");


    let tree = prover.read().await.incremental;

    let origin_conf = origin_chains.get(db_clone.domain()).unwrap().clone();

    let c = Checkpoint {
        root: tree.root(),
        index: tree.index(),
        merkle_tree_hook_address: origin_conf.addresses.merkle_tree_hook,
        mailbox_domain: domain.id(),
    };

    let cm = CheckpointWithMessageId {
        checkpoint: c,
        message_id: message.id(),
    };

    let signed_check = signer.sign(cm).await.or_else(|err| Err(err.to_string()))?;

    const CTX: &str = "When fetching message proof";
    let proof = prover
        .read()
        .await
        .get_proof(tree.index(), tree.index())
        .context(CTX)
        .map_err(Report::from).expect("TODO: panic message");


    let mcm = MultisigSignedCheckpoint {
        checkpoint: cm,
        signatures: vec![signed_check.signature],
    };

    let metadata = MultisigMetadata::new(
        mcm,
        tree.index(),
        Some(proof),
    );

    // Finally, build submit arg and dispatch it to the submitter.
    let pending_msg = PendingMessage::from_persisted_retries(
        message.clone(),
        destination_ctxs[&destination].clone(),
    );

    let is_already_delivered = pending_msg.ctx
        .destination_mailbox
        .delivered(pending_msg.message.id())
        .await
        .context("checking message delivery status")
        .map_err(Report::from).expect("TODO: panic message");

    if is_already_delivered {
        let err_msg = "Message has already been delivered, marking as submitted.";
        debug!(err_msg);
        return Err(err_msg.to_string());
    }

    let provider = pending_msg.ctx.destination_mailbox.provider();

    // We cannot deliver to an address that is not a contract so check and drop if it isn't.
    let is_contract = provider.is_contract(&pending_msg.message.recipient).await.context("checking if message recipient is a contract").map_err(Report::from).expect("TODO: panic message");

    if !is_contract {
        let err_msg = "Dropping message because recipient is not a contract";
        info!(
            recipient=?pending_msg.message.recipient,
            err_msg
        );
        return Err(err_msg.to_string());
    }

    let build_token = |token: &MetadataToken| -> eyre::Result<Vec<u8>> {
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
    let token_layout = vec![
        MetadataToken::CheckpointMerkleTreeHook,
        MetadataToken::MessageMerkleLeafIndex,
        MetadataToken::MessageId,
        MetadataToken::MerkleProof,
        MetadataToken::CheckpointIndex,
        MetadataToken::Signatures,
    ];
    let metas: Vec<Vec<u8>> = token_layout
        .iter()
        .map(build_token)
        .collect::<eyre::Result<Vec<Vec<u8>>, _>>()
        .map_err(Report::from)
        .expect("TODO: panic message");

    //let metas: eyre::Result<Vec<Vec<u8>>> = token_layout.iter().map(build_token).collect().map_err(Report::from).expect("TODO: panic message");

    let meta: Vec<u8> = metas.into_iter().flatten().collect();

    let tx_cost_estimate = pending_msg.ctx
        .destination_mailbox
        .process_estimate_costs(&pending_msg.message, &meta)
        .await
        .context("estimating costs for process call")
        .map_err(Report::from).expect("TODO: panic message");

    let Some(gas_limit) = pending_msg.ctx
        .origin_gas_payment_enforcer
        .message_meets_gas_payment_requirement(&pending_msg.message, &tx_cost_estimate)
        .await
        .context("checking if message meets gas payment requirement")
        .map_err(Report::from)
        .expect("TODO: panic message") else {
            let err_msg = "Gas payment requirement not met yet";
            info!(?tx_cost_estimate, err_msg);
            return Err(err_msg.to_string());
        };

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

    let response_body = ValidityResponse {
        message: pending_msg.message,
        metadata: meta,
        gas_limit,
        error: None,
    };
    Ok(response_body)
}

pub async fn batch_check_validity_request(mut req: Request<State>) -> tide::Result {
    let validity_batch_request : ValidityBatchRequest = req.body_json().await?;
    let state = req.state().clone();

    let batch_response = batch_check_validity(&validity_batch_request, &state).await;

    let mut res = Response::new(200);
    res.set_body(Body::from_json(&batch_response)?);
    Ok(res)
}
pub async fn batch_check_validity(validity_batch_request: &ValidityBatchRequest, state: &State) -> ValidityBatchResponse {
    let mut response_body = ValidityBatchResponse {
        responses: Vec::new()
    };

    for request in &validity_batch_request.requests {
        let validity_response = check_validity(&request, &state).await.unwrap_or_else(|e| new_validity_response_error(e));

        response_body.responses.push(validity_response)
    }

    response_body
}
