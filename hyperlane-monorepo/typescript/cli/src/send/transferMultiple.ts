import { input } from '@inquirer/prompts';
import { BigNumber, ethers } from 'ethers';

import {
  ERC20__factory,
  HypERC20Collateral__factory,
} from '@hyperlane-xyz/core';
import {
  // ChainMap,
  // ChainMetadata,
  ChainName,
  EvmHypCollateralAdapter,
  HyperlaneContractsMap,
  HyperlaneCore,
  MultiProtocolProvider,
  MultiProvider,
  TokenType,
} from '@hyperlane-xyz/sdk';
import { Address, sleep, timeout } from '@hyperlane-xyz/utils';

import { log, logBlue, logGreen } from '../../logger.js';
import { MINIMUM_TEST_SEND_GAS } from '../consts.js';
import { CommandContext, getContext, getMergedContractAddresses } from '../context.js';
import { runPreflightChecks } from '../deploy/utils.js';
import { assertNativeBalances, assertTokenBalance } from '../utils/balances.js';
import { runSingleChainSelectionStep } from '../utils/chains.js';

export async function sendTestTransferMulti({
  keys,
  numTxPerKey,
  chainConfigPath,
  coreArtifactsPath,
  origin,
  destination,
  routerAddress,
  tokenType,
  wei,
  recipient,
  timeoutSec,
  skipWaitForDelivery,
}: {
  keys: string;
  numTxPerKey: number;
  chainConfigPath: string;
  coreArtifactsPath?: string;
  origin?: ChainName;
  destination?: ChainName;
  routerAddress?: Address;
  tokenType: TokenType;
  wei: string;
  recipient?: string;
  timeoutSec: number;
  skipWaitForDelivery: boolean;
}) {
  const signerCtxs: CommandContext<{
    chainConfigPath: string;
    coreConfig: {
        coreArtifactsPath: string | undefined;
    };
    keyConfig: {
        key: string;
    };
}>[] = [];
  const keyArr = keys.split(',');
  for(let i=0; i<keyArr.length; i++) {
    let key = keyArr[i];
    const ctx =
      await getContext({
        chainConfigPath,
        coreConfig: { coreArtifactsPath },
        keyConfig: { key },
      });
    signerCtxs.push(ctx);
  }

  if(signerCtxs.length == 0) {
    throw new Error("no key provided") 
  } 

  const customChains = signerCtxs[0].customChains;

  if (!origin) {
    origin = await runSingleChainSelectionStep(
      customChains,
      'Select the origin chain',
    );
  }

  if (!destination) {
    destination = await runSingleChainSelectionStep(
      customChains,
      'Select the destination chain',
    );
  }

  if (!routerAddress) {
    routerAddress = await input({
      message: 'Please specify the router address',
    });
  }

  const baseNonces: {[key:string]:  number} = {};
  for(const ctx of signerCtxs) {
    const multiProvider = ctx.multiProvider;
    const signer = ctx.signer;
    // asset check
    if (tokenType === TokenType.collateral) {
      await assertTokenBalance(
        multiProvider,
        signer,
        origin,
        routerAddress,
        wei.toString(),
      );
    } else if (tokenType === TokenType.native) {
      await assertNativeBalances(multiProvider, signer, [origin], wei.toString());
    } else {
      throw new Error(
        'Only collateral and native token types are currently supported in the CLI. For synthetic transfers, try the Warp UI.',
      );
    }

    // preflight check
    await runPreflightChecks({
      origin,
      remotes: [destination],
      multiProvider,
      signer,
      minGas: MINIMUM_TEST_SEND_GAS,
      chainsToGasCheck: [origin],
    });

    // populate nonces
    const signerAddress = await signer.getAddress();
    const provider = multiProvider.getProvider(origin);
    const nonce = await provider.getTransactionCount(signerAddress)
    baseNonces[signerAddress] = nonce
  }
  logBlue("===========nonce info=============")
  for(const addr in baseNonces) {
    const nonce = baseNonces[addr]
    logBlue(`account ${addr}: ${nonce}`)
  }

  if(Object.keys(baseNonces).length == 0) {
    throw new Error("cannot fetch nonces of any account")
  }

  logGreen(`Num txns per key: ${numTxPerKey}`)

  const parentTasks: Promise<void>[] = [];
  const tasks: Promise<void>[] = [];
  for(const ctx of signerCtxs) {
    const signer = ctx.signer;
    const addr = await signer.getAddress();
    if(!Object.keys(baseNonces).includes(addr)) {
      continue
    }
    const t = (async () => {
      for(let i=0; i<numTxPerKey; i++) {
        const nonce = baseNonces[addr];
        const multiProvider = ctx.multiProvider;
        const coreArtifacts = ctx.coreArtifacts;
        await sleep(50);
        const task = timeout(
          executeDelivery({
            origin,
            destination,
            routerAddress,
            tokenType,
            wei,
            recipient,
            signer,
            multiProvider,
            coreArtifacts,
            skipWaitForDelivery,
            nonce: nonce + i,
          }),
          timeoutSec * 1000,
          'Timed out waiting for messages to be delivered',
        );

        tasks.push(task);
      }
    })()

    parentTasks.push(t);
  }

  await Promise.all(parentTasks);
  logGreen('all transactions for each account has been sent, waiting to be included')
  await Promise.all(tasks);
}

async function executeDelivery({
  origin,
  destination,
  routerAddress,
  tokenType,
  wei,
  recipient,
  multiProvider,
  signer,
  coreArtifacts,
  skipWaitForDelivery,
  nonce,
}: {
  origin: ChainName;
  destination: ChainName;
  routerAddress: Address;
  tokenType: TokenType;
  wei: string;
  recipient?: string;
  multiProvider: MultiProvider;
  signer: ethers.Signer;
  coreArtifacts?: HyperlaneContractsMap<any>;
  skipWaitForDelivery: boolean;
  nonce?: number;
}) {
  const signerAddress = await signer.getAddress();
  recipient ||= signerAddress;

  const mergedContractAddrs = getMergedContractAddresses(coreArtifacts);

  const core = HyperlaneCore.fromAddressesMap(
    mergedContractAddrs,
    multiProvider,
  );

  const provider = multiProvider.getProvider(origin);
  const connectedSigner = signer.connect(provider);

  if (tokenType === TokenType.collateral) {
    const wrappedToken = await getWrappedToken(routerAddress, provider);
    const token = ERC20__factory.connect(wrappedToken, connectedSigner);
    const approval = await token.allowance(signerAddress, routerAddress);
    if (approval.lt(wei)) {
      const approveTx = await token.approve(routerAddress, wei);
      await approveTx.wait();
    }
  }

  // TODO move next section into MultiProtocolTokenApp when it exists
  const adapter = new EvmHypCollateralAdapter(
    origin,
    MultiProtocolProvider.fromMultiProvider(multiProvider),
    { token: routerAddress },
  );
  const destinationDomain = multiProvider.getDomainId(destination);
  const gasPayment = await adapter.quoteGasPayment(destinationDomain);
  const txValue =
    tokenType === TokenType.native
      ? BigNumber.from(gasPayment).add(wei).toString()
      : gasPayment;
  const transferTx = await adapter.populateTransferRemoteTx({
    weiAmountOrId: wei,
    destination: destinationDomain,
    recipient,
    txValue,
  });
  // override nonce
  if(nonce) {
    transferTx.nonce = nonce;
  }

  const txResponse = await connectedSigner.sendTransaction(transferTx);
  const txReceipt = await multiProvider.handleTx(origin, txResponse);

  const message = core.getDispatchedMessages(txReceipt)[0];
  logBlue(`Sent message from ${origin} to ${recipient} on ${destination}.`);
  logBlue(`Message ID: ${message.id}`);

  if (skipWaitForDelivery) return;

  // Max wait 10 minutes
  await core.waitForMessageProcessed(txReceipt, 10000, 60);
  logGreen(`Transfer sent to destination chain!`);
}

async function getWrappedToken(
  address: Address,
  provider: ethers.providers.Provider,
): Promise<Address> {
  try {
    const contract = HypERC20Collateral__factory.connect(address, provider);
    const wrappedToken = await contract.wrappedToken();
    if (ethers.utils.isAddress(wrappedToken)) return wrappedToken;
    else throw new Error('Invalid wrapped token address');
  } catch (error) {
    log('Error getting wrapped token', error);
    throw new Error(
      `Could not get wrapped token from router address ${address}`,
    );
  }
}
