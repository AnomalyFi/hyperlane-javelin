use tide::{Request, Response, Body};
use tracing::debug;
use hyperlane_core::{Checkpoint, CheckpointWithMessageId, HyperlaneSignerExt};
use crate::apiserver::{State, ValidityRequest};
use crate::msg::pending_message::PendingMessage;


pub async fn check_validity(mut req: Request<State>) -> tide::Result {
    let ValidityRequest { domain, message, insertion } = req.body_json().await?;

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

    // Finally, build the submit arg and dispatch it to the submitter.
    let pending_msg = PendingMessage::from_persisted_retries(
        message,
        destination_ctxs[&destination].clone(),
    );



    //MerkleTreeProcessor Logic
    prover
        .write()
        .await
        .ingest_message_id(insertion.message_id())
        .await?;


    //TODO The builder submits their transaction via the request

    // create a snapshot/checkpoint of the rocksdb

    // New instances of MessageProcessor and MerkleTreeProcessor

    // Process the messsage getting the root we need
    // and include the validator logic to sign the root here.


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
        message_id: insertion.message_id(),
    };

    let signed_check = signer.sign(cm).await?;

    // Return the complete transaction to the builder who includes it in a block.
    let mut res = Response::new(200);
    res.set_body(Body::from_json(&signed_check)?);
    Ok(res)

}