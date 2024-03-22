use std::time::Instant;
use ethers::abi::Token;
use eyre::Context;
use tide::{Request, Response, Body};
use tracing::{debug, info, trace};
use hyperlane_core::{Checkpoint, CheckpointWithMessageId, MultisigSignedCheckpoint, HyperlaneSignerExt};
use crate::apiserver::{State, ValidityRequest};
use crate::msg::pending_message::PendingMessage;
use crate::msg::metadata::multisig::{MetadataToken, MultisigMetadata};
use crate::msg::pending_operation::PendingOperationResult;


pub async fn check_validity(mut req: Request<State>) -> tide::Result {
    let ValidityRequest { domain, message } = req.body_json().await?;

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
    let prover = prover_syncs.get(&domain).cloned().ok_or_else(|| tide::Error::from_str(404, "Prover not found"))?;

    // Get the database from the dbs map based on the domain.
    let db_clone = dbs.get(&domain).ok_or_else(|| tide::Error::from_str(404, "Database not found"))?.clone();

    // let snap = db_clone.snapshot_db();

    //MessageProcessor Logic

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
        .collect();


    let destination = message.destination;

    // Skip if not whitelisted.
    if !whitelist.msg_matches(&message, true) {
        debug!(?message, whitelist=?whitelist, "Message not whitelisted, skipping");

        let mut res = Response::new(404);
        Ok(res)

    }

    // Skip if the message is blacklisted
    if blacklist.msg_matches(&message, false) {
        debug!(?message, blacklist=?blacklist, "Message blacklisted, skipping");

        let mut res = Response::new(404);
        Ok(res)
    }

    // Skip if the message is intended for this origin
    if destination == db_clone.domain().id() {
        debug!(?message, "Message destined for self, skipping");

        let mut res = Response::new(404);
        Ok(res)
    }

    // Skip if the message is intended for a destination we do not service
    // if !self.send_channels.contains_key(&destination) {
    //     debug!(?message, "Message destined for unknown domain, skipping");
    //     self.message_nonce += 1;
    //     return Ok(());
    // }

    debug!(%message, "Sending message to submitter");



    //MerkleTreeProcessor Logic
    prover
        .write()
        .await
        .ingest_message_id(message.id())
        .await?;


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

    let signed_check = signer.sign(cm).await?;

    const CTX: &str = "When fetching message proof";
    //TODO this is completely unneccessary but was kept from when validators and relayers weren't the same entity so the index might not match in those cases
    let proof = prover
        .read()
        .await
        .get_proof(tree.index(), tree.index())
        .context(CTX)?;

    let mcm = MultisigSignedCheckpoint {
        checkpoint: cm,
        signatures: vec![signed_check.signature],
    };

    let metadata = MultisigMetadata::new(
        mcm,
        tree.index(),
        Some(proof),
    );


    // Finally, build the submit arg and dispatch it to the submitter.
    let pending_msg = PendingMessage::from_persisted_retries(
        message,
        destination_ctxs[&destination].clone(),
    );




    //TODO implement all this code to avoid a message getting resubmitted. This will play into our checkout system that tracks the messaages within one block



    // If the message has already been processed, e.g. due to another apiserver having
    // already processed, then mark it as already-processed, and move on to
    // the next tick.
    let is_already_delivered = pending_msg.ctx
        .destination_mailbox
        .delivered(pending_msg.message.id())
        .await
        .context("checking message delivery status")?;

    if is_already_delivered {
        debug!("Message has already been delivered, marking as submitted.");

        let mut res = Response::new(404);
        Ok(res)
    }

    let provider = pending_msg.ctx.destination_mailbox.provider();

    // We cannot deliver to an address that is not a contract so check and drop if it isn't.
    let is_contract = provider.is_contract(&pending_msg.message.recipient).await.context("checking if message recipient is a contract")?;


    if !is_contract {
        info!(
                recipient=?pending_msg.message.recipient,
                "Dropping message because recipient is not a contract"
            );
        let mut res = Response::new(404);
        Ok(res)
    }

    // let ism_address =  pending_msg.ctx
    //     .destination_mailbox
    //     .recipient_ism(pending_msg.message.recipient)
    //     .await
    //     .context("fetching ISM address. Potentially malformed recipient ISM address.")?;


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
    let metas: eyre::Result<Vec<Vec<u8>>> = token_layout.iter().map(build_token).collect();

    let meta: Vec<u8> = metas?.into_iter().flatten().collect();


    // Estimate transaction costs for the process call. If there are issues, it's
    // likely that gas estimation has failed because the message is
    // reverting. This is defined behavior, so we just log the error and
    // move onto the next tick.
    let tx_cost_estimate = pending_msg.ctx
        .destination_mailbox
        .process_estimate_costs(&pending_msg.message, &meta)
        .await
        .context("estimating costs for process call")?;

    // If the gas payment requirement hasn't been met, move to the next tick.
    let Some(gas_limit) = pending_msg.ctx
        .origin_gas_payment_enforcer
        .message_meets_gas_payment_requirement(&pending_msg.message, &tx_cost_estimate)
        .await
        .context("checking if message meets gas payment requirement")? else {
        info!(?tx_cost_estimate, "Gas payment requirement not met yet");
        let mut res = Response::new(404);
        Ok(res)
    };

    // Go ahead and attempt processing of message to destination chain.
    debug!(
            ?gas_limit,
            "Gas payment requirement met, ready to process message"
        );

    let gas_limit = tx_cost_estimate.gas_limit;

    if let Some(max_limit) = pending_msg.ctx.transaction_gas_limit {
        if gas_limit > max_limit {
            info!("Message delivery estimated gas exceeds max gas limit");
            let mut res = Response::new(404);
            Ok(res)
        }
    }

    let submission_data = Some(Box::new(crate::msg::pending_message::SubmissionData {
        metadata: meta,
        gas_limit,
    }));


    //TODO return pending_metadata.message, submission_data.metadata, submission_data.gas_limit



    // Return the complete transaction to the builder who includes it in a block.
    let mut res = Response::new(200);
    res.set_body(Body::from_json(&signed_check)?);
    Ok(res)

}