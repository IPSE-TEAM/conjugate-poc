// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Proof of Capacity consensus for Substrate.
//!
//! To use this engine, you can need to have a struct that implements
//! `PocAlgorithm`. After that, pass an instance of the struct, along
//! with other necessary client references to `import_queue` to setup
//! the queue. Use the `start_mine` function for basic CPU mining.
//!
//! The auxiliary storage for PoC engine only stores the total difficulty.
//! For other storage requirements for particular PoC algorithm (such as
//! the actual difficulty for each particular blocks), you can take a client
//! reference in your `PocAlgorithm` implementation, and use a separate prefix
//! for the auxiliary storage. It is also possible to just use the runtime
//! as the storage, but it is not recommended as it won't work well with light
//! clients.

use std::sync::Arc;
use std::thread;
use std::collections::HashMap;
use client::{
	BlockOf, blockchain::{HeaderBackend, ProvideCache},
	block_builder::api::BlockBuilder as BlockBuilderApi, backend::AuxStore,
	well_known_cache_keys::Id as CacheKeyId,
};
use sr_primitives::Justification;
use sr_primitives::generic::{BlockId, Digest, DigestItem};
use sr_primitives::traits::{Block as BlockT, Header as HeaderT, ProvideRuntimeApi};
use srml_timestamp::{TimestampInherentData, InherentError as TIError};
use poc_primitives::{Seal, TotalDifficulty, POC_ENGINE_ID,NonceData};
use primitives::H256;
use inherents::{InherentDataProviders, InherentData};
use consensus_common::{
	BlockImportParams, BlockOrigin, ForkChoiceStrategy, SyncOracle, Environment, Proposer,
	SelectChain,
};
use consensus_common::import_queue::{BoxBlockImport, BasicQueue, Verifier};
use codec::{Encode, Decode};
use log::*;

/// Auxiliary storage prefix for PoC engine.
pub const POC_AUX_PREFIX: [u8; 4] = *b"PoC:";

/// Get the auxiliary storage key used by engine to store total difficulty.
fn aux_key(hash: &H256) -> Vec<u8> {
	POC_AUX_PREFIX.iter().chain(&hash[..])
		.cloned().collect::<Vec<_>>()
}

/// Auxiliary storage data for PoC.
#[derive(Encode, Decode, Clone, Debug, Default)]
pub struct PocAux<Difficulty> {
	/// Difficulty of the current block.
	pub difficulty: Difficulty,
	/// Total difficulty up to current block.
	pub total_difficulty: Difficulty,
}

impl<Difficulty> PocAux<Difficulty> where
	Difficulty: Decode + Default,
{
	/// Read the auxiliary from client.
	pub fn read<C: AuxStore>(client: &C, hash: &H256) -> Result<Self, String> {
		let key = aux_key(hash);

		match client.get_aux(&key).map_err(|e| format!("{:?}", e))? {
			Some(bytes) => Self::decode(&mut &bytes[..])
				.map_err(|e| format!("{:?}", e)),
			None => Ok(Self::default()),
		}
	}
}

/// Algorithm used for proof of capacity.
pub trait PocAlgorithm<B: BlockT> {
	/// Difficulty for the algorithm.
	type Difficulty: TotalDifficulty + Default + Encode + Decode + Ord + Clone + Copy;

	/// Get the next block's difficulty.
	fn difficulty(&self, parent: &BlockId<B>) -> Result<Self::Difficulty, String>;
	/// Verify proof of capacity against the given difficulty.
	fn verify(
		&self,
		parent: &BlockId<B>,
		pre_hash: &H256,
		seal: &Seal,
		difficulty: Self::Difficulty,
	) -> Result<bool, String>;
	/// Mine a seal that satisfy the given difficulty.
	fn mine(
		&self,
		parent: &BlockId<B>,
		pre_hash: &H256,
		difficulty: Self::Difficulty,
		round: u32,
	) -> Result<Option<Seal>, String>;
	/// Poc mine a NonceData that satisfy the given baseTarget
	fn poc_mine(
		&self,
		parent: &BlockId<B>,
		generation_sig: H256,
		baseTarget: Self::Difficulty, // baseTarget as the difficuty of PoW
	) -> Result<Option<NonceData>, String>;
	/// Poc verify proof of capacity against the given nonce
	fn poc_verify(
		&self,
		parent: &BlockId<B>,
		pre_hash: &H256,
		nonce_data: &NonceData,
		baseTarget: Self::Difficulty,
	) -> Result<bool, String>;
}

/// A verifier for PoC blocks.
pub struct PocVerifier<B: BlockT<Hash=H256>, C, S, Algorithm> {
	client: Arc<C>,
	algorithm: Algorithm,
	inherent_data_providers: inherents::InherentDataProviders,
	select_chain: Option<S>,
	check_inherents_after: <<B as BlockT>::Header as HeaderT>::Number,
}

impl<B: BlockT<Hash=H256>, C, S, Algorithm> PocVerifier<B, C, S, Algorithm> {
	pub fn new(
		client: Arc<C>,
		algorithm: Algorithm,
		check_inherents_after: <<B as BlockT>::Header as HeaderT>::Number,
		select_chain: Option<S>,
		inherent_data_providers: inherents::InherentDataProviders,
	) -> Self {
		Self { client, algorithm, inherent_data_providers, select_chain, check_inherents_after }
	}

	fn check_header(
		&self,
		mut header: B::Header,
		parent_block_id: BlockId<B>,
	) -> Result<(B::Header, Algorithm::Difficulty, DigestItem<H256>), String> where
		Algorithm: PocAlgorithm<B>,
	{
		let hash = header.hash();

		let (nonceData, inner_nonceData) = match header.digest_mut().pop() {
			Some(DigestItem::Seal(id, seal)) => {
				if id == POC_ENGINE_ID {
					(DigestItem::Seal(id, seal.clone()), seal)
				} else {
					return Err(format!("Header uses the wrong engine {:?}", id))
				}
			},
			_ => return Err(format!("Header {:?} is unsealed", hash)),
		};

		let pre_hash = header.hash();
		let difficulty = self.algorithm.difficulty(&parent_block_id)?;

		if !self.algorithm.poc_verify(
			&parent_block_id,
			&pre_hash,
			&inner_nonceData,
			difficulty,
		)? {
			return Err("PoC validation error: invalid nonceData".into());
		}

		Ok((header, difficulty, nonceData))
	}

	fn check_inherents(
		&self,
		block: B,
		block_id: BlockId<B>,
		inherent_data: InherentData,
		timestamp_now: u64,
	) -> Result<(), String> where
		C: ProvideRuntimeApi, C::Api: BlockBuilderApi<B>
	{
		const MAX_TIMESTAMP_DRIFT_SECS: u64 = 60;

		if *block.header().number() < self.check_inherents_after {
			return Ok(())
		}

		let inherent_res = self.client.runtime_api().check_inherents(
			&block_id,
			block,
			inherent_data,
		).map_err(|e| format!("{:?}", e))?;

		if !inherent_res.ok() {
			inherent_res
				.into_errors()
				.try_for_each(|(i, e)| match TIError::try_from(&i, &e) {
					Some(TIError::ValidAtTimestamp(timestamp)) => {
						if timestamp > timestamp_now + MAX_TIMESTAMP_DRIFT_SECS {
							return Err("Rejecting block too far in future".into());
						}

						Ok(())
					},
					Some(TIError::Other(e)) => Err(e.into()),
					None => Err(self.inherent_data_providers.error_to_string(&i, &e)),
				})
		} else {
			Ok(())
		}
	}
}

impl<B: BlockT<Hash=H256>, C, S, Algorithm> Verifier<B> for PocVerifier<B, C, S, Algorithm> where
	C: ProvideRuntimeApi + Send + Sync + HeaderBackend<B> + AuxStore + ProvideCache<B> + BlockOf,
	C::Api: BlockBuilderApi<B>,
	S: SelectChain<B>,
	Algorithm: PocAlgorithm<B> + Send + Sync,
{
	fn verify(
		&mut self,
		origin: BlockOrigin,
		header: B::Header,
		justification: Option<Justification>,
		mut body: Option<Vec<B::Extrinsic>>,
	) -> Result<(BlockImportParams<B>, Option<Vec<(CacheKeyId, Vec<u8>)>>), String> {
		let inherent_data = self.inherent_data_providers
			.create_inherent_data().map_err(String::from)?;
		let timestamp_now = inherent_data.timestamp_inherent_data().map_err(String::from)?;

		let best_hash = match self.select_chain.as_ref() {
			Some(select_chain) => select_chain.best_chain()
				.map_err(|e| format!("Fetch best chain failed via select chain: {:?}", e))?
				.hash(),
			None => self.client.info().best_hash,
		};
		let hash = header.hash();
		let parent_hash = *header.parent_hash();
		let best_aux = PocAux::read(self.client.as_ref(), &best_hash)?;
		let mut aux = PocAux::read(self.client.as_ref(), &parent_hash)?;

		let (checked_header, difficulty, nonceData) = self.check_header(
			header,
			BlockId::Hash(parent_hash),
		)?;
		aux.difficulty = difficulty;
		aux.total_difficulty.increment(difficulty);

		if let Some(inner_body) = body.take() {
			let block = B::new(checked_header.clone(), inner_body);

			self.check_inherents(
				block.clone(),
				BlockId::Hash(parent_hash),
				inherent_data,
				timestamp_now
			)?;

			let (_, inner_body) = block.deconstruct();
			body = Some(inner_body);
		}
		let key = aux_key(&hash);
		let import_block = BlockImportParams {
			origin,
			header: checked_header,
			post_digests: vec![nonceData],
			body,
			finalized: false,
			justification,
			auxiliary: vec![(key, Some(aux.encode()))],
			fork_choice: ForkChoiceStrategy::Custom(aux.total_difficulty > best_aux.total_difficulty),
		};

		Ok((import_block, None))
	}
}

/// Register the PoC inherent data provider, if not registered already.
pub fn register_poc_inherent_data_provider(
	inherent_data_providers: &InherentDataProviders,
) -> Result<(), consensus_common::Error> {
	if !inherent_data_providers.has_provider(&srml_timestamp::INHERENT_IDENTIFIER) {
		inherent_data_providers
			.register_provider(srml_timestamp::InherentDataProvider)
			.map_err(Into::into)
			.map_err(consensus_common::Error::InherentData)
	} else {
		Ok(())
	}
}

/// The PoC import queue type.
pub type PocImportQueue<B> = BasicQueue<B>;

/// Import queue for PoC engine.
pub fn import_queue<B, C, S, Algorithm>(
	block_import: BoxBlockImport<B>,
	client: Arc<C>,
	algorithm: Algorithm,
	check_inherents_after: <<B as BlockT>::Header as HeaderT>::Number,
	select_chain: Option<S>,
	inherent_data_providers: InherentDataProviders,
) -> Result<PocImportQueue<B>, consensus_common::Error> where
	B: BlockT<Hash=H256>,
	C: ProvideRuntimeApi + HeaderBackend<B> + BlockOf + ProvideCache<B> + AuxStore,
	C: Send + Sync + AuxStore + 'static,
	C::Api: BlockBuilderApi<B>,
	Algorithm: PocAlgorithm<B> + Send + Sync + 'static,
	S: SelectChain<B> + 'static,
{
	register_poc_inherent_data_provider(&inherent_data_providers)?;

	let verifier = PocVerifier::new(
		client.clone(),
		algorithm,
		check_inherents_after,
		select_chain,
		inherent_data_providers,
	);

	Ok(BasicQueue::new(
		verifier,
		block_import,
		None,
		None
	))
}

/// Start the background mining thread for PoC. Note that because PoC mining
/// is CPU-intensive, it is not possible to use an async future to define this.
/// However, it's not recommended to use background threads in the rest of the
/// codebase.
///
/// `preruntime` is a parameter that allows a custom additional pre-runtime
/// digest to be inserted for blocks being built. This can encode authorship
/// information, or just be a graffiti. `round` is for number of rounds the
/// CPU miner runs each time. This parameter should be tweaked so that each
/// mining round is within sub-second time.
pub fn start_mine<B: BlockT<Hash=H256>, C, Algorithm, E, SO, S>(
	mut block_import: BoxBlockImport<B>,
	client: Arc<C>,
	algorithm: Algorithm,
	mut env: E,
	preruntime: Option<Vec<u8>>,
	round: u32,
	mut sync_oracle: SO,
	build_time: std::time::Duration,
	select_chain: Option<S>,
	inherent_data_providers: inherents::InherentDataProviders,
) where
	C: HeaderBackend<B> + AuxStore + 'static,
	Algorithm: PocAlgorithm<B> + Send + Sync + 'static,
	E: Environment<B> + Send + Sync + 'static,
	E::Error: std::fmt::Debug,
	SO: SyncOracle + Send + Sync + 'static,
	S: SelectChain<B> + 'static,
{
	if let Err(_) = register_poc_inherent_data_provider(&inherent_data_providers) {
		warn!("Registering inherent data provider for timestamp failed");
	}

	thread::spawn(move || {
		loop {
			match mine_loop(
				&mut block_import,
				client.as_ref(),
				&algorithm,
				&mut env,
				preruntime.as_ref(),
				round,
				&mut sync_oracle,
				build_time.clone(),
				select_chain.as_ref(),
				&inherent_data_providers
			) {
				Ok(()) => (),
				Err(e) => error!(
					"Mining block failed with {:?}. Sleep for 1 second before restarting...",
					e
				),
			}
			std::thread::sleep(std::time::Duration::new(1, 0));
		}
	});
}

fn mine_loop<B: BlockT<Hash=H256>, C, Algorithm, E, SO, S>(
	block_import: &mut BoxBlockImport<B>,
	client: &C,
	algorithm: &Algorithm,
	env: &mut E,
	preruntime: Option<&Vec<u8>>,
	round: u32,
	sync_oracle: &mut SO,
	build_time: std::time::Duration,
	select_chain: Option<&S>,
	inherent_data_providers: &inherents::InherentDataProviders,
) -> Result<(), String> where
	C: HeaderBackend<B> + AuxStore,
	Algorithm: PocAlgorithm<B>,
	E: Environment<B>,
	E::Error: std::fmt::Debug,
	SO: SyncOracle,
	S: SelectChain<B>,
{
	'outer: loop {
		if sync_oracle.is_major_syncing() {
			debug!(target: "poc", "Skipping proposal due to sync.");
			std::thread::sleep(std::time::Duration::new(1, 0));
			continue 'outer
		}

		let (best_hash, best_header) = match select_chain {
			Some(select_chain) => {
				let header = select_chain.best_chain()
					.map_err(|e| format!("Fetching best header failed using select chain: {:?}", e))?;
				let hash = header.hash();
				(hash, header)
			},
			None => {
				let hash = client.info().best_hash;
				let header = client.header(BlockId::Hash(hash))
					.map_err(|e| format!("Fetching best header failed: {:?}", e))?
					.ok_or("Best header does not exist")?;
				(hash, header)
			},
		};
		let mut aux = PocAux::read(client, &best_hash)?;
		let mut proposer = env.init(&best_header).map_err(|e| format!("{:?}", e))?;

		let inherent_data = inherent_data_providers
			.create_inherent_data().map_err(String::from)?;
		let mut inherent_digest = Digest::default();
		if let Some(preruntime) = &preruntime {
			inherent_digest.push(DigestItem::PreRuntime(POC_ENGINE_ID, preruntime.to_vec()));
		}
		let block = futures::executor::block_on(proposer.propose(
			inherent_data,
			inherent_digest,
			build_time.clone(),
		)).map_err(|e| format!("Block proposing error: {:?}", e))?;

		let (header, body) = block.deconstruct();
		// let (difficulty, seal) = {
		let (difficulty,nonceData) = {
			let difficulty = algorithm.difficulty(
				&BlockId::Hash(best_hash),
			)?;

			loop {
				// let seal = algorithm.mine(
				// 	&BlockId::Hash(best_hash),
				// 	&header.hash(),
				// 	difficulty,
				// 	round,
				// )?;
				let nonceData = algorithm.poc_mine(
					&BlockId::Hash(best_hash),
					header.hash(),
					difficulty,
				)?;

				// if let Some(seal) = seal {
				// 	break (difficulty, seal)
				// }
				if let Some(nonceData) = nonceData {
					break (difficulty,nonceData)
				}

				if best_hash != client.info().best_hash {
					continue 'outer
				}
			}
		};


		aux.difficulty = difficulty;
		aux.total_difficulty.increment(difficulty);
		let hash = {
			let mut header = header.clone();
			header.digest_mut().push(DigestItem::Seal(POC_ENGINE_ID, nonceData.clone()));
			header.hash()
		};

		let key = aux_key(&hash);
		let best_hash = match select_chain {
			Some(select_chain) => select_chain.best_chain()
				.map_err(|e| format!("Fetch best hash failed via select chain: {:?}", e))?
				.hash(),
			None => client.info().best_hash,
		};
		let best_aux = PocAux::<Algorithm::Difficulty>::read(client, &best_hash)?;

		// if the best block has changed in the meantime drop our proposal
		if best_aux.total_difficulty > aux.total_difficulty {
			continue 'outer
		}

		let import_block = BlockImportParams {
			origin: BlockOrigin::Own,
			header,
			justification: None,
			post_digests: vec![DigestItem::Seal(POC_ENGINE_ID, nonceData)],
			body: Some(body),
			finalized: false,
			auxiliary: vec![(key, Some(aux.encode()))],
			fork_choice: ForkChoiceStrategy::Custom(true),
		};

		block_import.import_block(import_block, HashMap::default())
			.map_err(|e| format!("Error with block built on {:?}: {:?}", best_hash, e))?;
	}
}
