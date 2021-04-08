// This file is part of Substrate.

// Copyright (C) 2018-2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{
	CodeHash, Event, Config, Pallet as Contracts,
	BalanceOf, ContractInfo, gas::GasMeter, rent::Rent, storage::Storage,
	Error, ContractInfoOf, Schedule, AliveContractInfo, AccountCounter,
};
use sp_core::crypto::UncheckedFrom;
use sp_std::{
	prelude::*,
	marker::PhantomData,
	mem,
};
use sp_runtime::{Perbill, traits::{Convert, Saturating}};
use frame_support::{
	dispatch::{DispatchResult, DispatchError},
	storage::{with_transaction, TransactionOutcome},
	traits::{ExistenceRequirement, Currency, Time, Randomness, Get},
	weights::Weight,
	ensure,
};
use pallet_contracts_primitives::{ErrorOrigin, ExecError, ExecReturnValue, ExecResult};

pub type AccountIdOf<T> = <T as frame_system::Config>::AccountId;
pub type MomentOf<T> = <<T as Config>::Time as Time>::Moment;
pub type SeedOf<T> = <T as frame_system::Config>::Hash;
pub type BlockNumberOf<T> = <T as frame_system::Config>::BlockNumber;
pub type StorageKey = [u8; 32];

/// A type that represents a topic of an event. At the moment a hash is used.
pub type TopicOf<T> = <T as frame_system::Config>::Hash;

/// Information needed for rent calculations that can be requested by a contract.
#[derive(codec::Encode)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub struct RentParams<T: Config> {
	/// The total balance of the contract. Includes the balance transferred from the caller.
	total_balance: BalanceOf<T>,
	/// The free balance of the contract. Includes the balance transferred from the caller.
	free_balance: BalanceOf<T>,
	/// See crate [`Contracts::subsistence_threshold()`].
	subsistence_threshold: BalanceOf<T>,
	/// See crate [`Config::DepositPerContract`].
	deposit_per_contract: BalanceOf<T>,
	/// See crate [`Config::DepositPerStorageByte`].
	deposit_per_storage_byte: BalanceOf<T>,
	/// See crate [`Config::DepositPerStorageItem`].
	deposit_per_storage_item: BalanceOf<T>,
	/// See crate [`Ext::rent_allowance()`].
	rent_allowance: BalanceOf<T>,
	/// See crate [`Config::RentFraction`].
	rent_fraction: Perbill,
	/// See crate [`AliveContractInfo::storage_size`].
	storage_size: u32,
	/// See crate [`Executable::aggregate_code_len()`].
	code_size: u32,
	/// See crate [`Executable::refcount()`].
	code_refcount: u32,
	/// Reserved for backwards compatible changes to this data structure.
	_reserved: Option<()>,
}

impl<T> RentParams<T>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
{
	fn new<E: Executable<T>>(
		account_id: &T::AccountId,
		contract: &AliveContractInfo<T>,
		executable: &E
	) -> Self {
		Self {
			total_balance: T::Currency::total_balance(account_id),
			free_balance: T::Currency::free_balance(account_id),
			subsistence_threshold: <Contracts<T>>::subsistence_threshold(),
			deposit_per_contract: T::DepositPerContract::get(),
			deposit_per_storage_byte: T::DepositPerStorageByte::get(),
			deposit_per_storage_item: T::DepositPerStorageItem::get(),
			rent_allowance: contract.rent_allowance,
			rent_fraction: T::RentFraction::get(),
			storage_size: contract.storage_size,
			code_size: executable.aggregate_code_len(),
			code_refcount: executable.refcount(),
			_reserved: None,
		}
	}
}

/// We cannot derive `Default` because `T` does not necessarily implement `Default`.
#[cfg(test)]
impl<T: Config> Default for RentParams<T> {
	fn default() -> Self {
		Self {
			total_balance: Default::default(),
			free_balance: Default::default(),
			subsistence_threshold: Default::default(),
			deposit_per_contract: Default::default(),
			deposit_per_storage_byte: Default::default(),
			deposit_per_storage_item: Default::default(),
			rent_allowance: Default::default(),
			rent_fraction: Default::default(),
			storage_size: Default::default(),
			code_size: Default::default(),
			code_refcount: Default::default(),
			_reserved: Default::default(),
		}
	}
}

/// An interface that provides access to the external environment in which the
/// smart-contract is executed.
///
/// This interface is specialized to an account of the executing code, so all
/// operations are implicitly performed on that account.
///
/// # Note
///
/// This trait is sealed and cannot be implemented by downstream crates.
pub trait Ext: sealing::Sealed {
	type T: Config;

	/// Call (possibly transferring some amount of funds) into the specified account.
	///
	/// Returns the original code size of the called contract.
	///
	/// # Return Value
	///
	/// Result<(ExecReturnValue, CodeSize), (ExecError, CodeSize)>
	fn call(
		&mut self,
		gas_limit: Weight,
		to: AccountIdOf<Self::T>,
		value: BalanceOf<Self::T>,
		input_data: Vec<u8>,
	) -> Result<(ExecReturnValue, u32), (ExecError, u32)>;

	/// Instantiate a contract from the given code.
	///
	/// Returns the original code size of the called contract.
	/// The newly created account will be associated with `code`. `value` specifies the amount of value
	/// transferred from this to the newly created account (also known as endowment).
	///
	/// # Return Value
	///
	/// Result<(AccountId, ExecReturnValue, CodeSize), (ExecError, CodeSize)>
	fn instantiate(
		&mut self,
		gas_limit: Weight,
		code: CodeHash<Self::T>,
		value: BalanceOf<Self::T>,
		input_data: Vec<u8>,
		salt: &[u8],
	) -> Result<(AccountIdOf<Self::T>, ExecReturnValue, u32), (ExecError, u32)>;

	/// Transfer all funds to `beneficiary` and delete the contract.
	///
	/// Returns the original code size of the terminated contract.
	/// Since this function removes the self contract eagerly, if succeeded, no further actions should
	/// be performed on this `Ext` instance.
	///
	/// This function will fail if the same contract is present on the contract
	/// call stack.
	///
	/// # Return Value
	///
	/// Result<CodeSize, (DispatchError, CodeSize)>
	fn terminate(
		&mut self,
		beneficiary: &AccountIdOf<Self::T>,
	) -> Result<u32, (DispatchError, u32)>;

	/// Restores the given destination contract sacrificing the current one.
	///
	/// Since this function removes the self contract eagerly, if succeeded, no further actions should
	/// be performed on this `Ext` instance.
	///
	/// This function will fail if the same contract is present
	/// on the contract call stack.
	///
	/// # Return Value
	///
	/// Result<(CallerCodeSize, DestCodeSize), (DispatchError, CallerCodeSize, DestCodesize)>
	fn restore_to(
		&mut self,
		dest: AccountIdOf<Self::T>,
		code_hash: CodeHash<Self::T>,
		rent_allowance: BalanceOf<Self::T>,
		delta: Vec<StorageKey>,
	) -> Result<(u32, u32), (DispatchError, u32, u32)>;

	/// Transfer some amount of funds into the specified account.
	fn transfer(
		&mut self,
		to: &AccountIdOf<Self::T>,
		value: BalanceOf<Self::T>,
	) -> DispatchResult;

	/// Returns the storage entry of the executing account by the given `key`.
	///
	/// Returns `None` if the `key` wasn't previously set by `set_storage` or
	/// was deleted.
	fn get_storage(&mut self, key: &StorageKey) -> Option<Vec<u8>>;

	/// Sets the storage entry by the given key to the specified value. If `value` is `None` then
	/// the storage entry is deleted.
	fn set_storage(&mut self, key: StorageKey, value: Option<Vec<u8>>) -> DispatchResult;

	/// Returns a reference to the account id of the caller.
	fn caller(&self) -> &AccountIdOf<Self::T>;

	/// Returns a reference to the account id of the current contract.
	fn address(&self) -> &AccountIdOf<Self::T>;

	/// Returns the balance of the current contract.
	///
	/// The `value_transferred` is already added.
	fn balance(&self) -> BalanceOf<Self::T>;

	/// Returns the value transferred along with this call or as endowment.
	fn value_transferred(&self) -> BalanceOf<Self::T>;

	/// Returns a reference to the timestamp of the current block
	fn now(&self) -> &MomentOf<Self::T>;

	/// Returns the minimum balance that is required for creating an account.
	fn minimum_balance(&self) -> BalanceOf<Self::T>;

	/// Returns the deposit required to create a tombstone upon contract eviction.
	fn tombstone_deposit(&self) -> BalanceOf<Self::T>;

	/// Returns a random number for the current block with the given subject.
	fn random(&self, subject: &[u8]) -> (SeedOf<Self::T>, BlockNumberOf<Self::T>);

	/// Deposit an event with the given topics.
	///
	/// There should not be any duplicates in `topics`.
	fn deposit_event(&mut self, topics: Vec<TopicOf<Self::T>>, data: Vec<u8>);

	/// Set rent allowance of the contract
	fn set_rent_allowance(&mut self, rent_allowance: BalanceOf<Self::T>);

	/// Rent allowance of the contract
	fn rent_allowance(&mut self) -> BalanceOf<Self::T>;

	/// Returns the current block number.
	fn block_number(&self) -> BlockNumberOf<Self::T>;

	/// Returns the maximum allowed size of a storage item.
	fn max_value_size(&self) -> u32;

	/// Returns the price for the specified amount of weight.
	fn get_weight_price(&self, weight: Weight) -> BalanceOf<Self::T>;

	/// Get a reference to the schedule used by the current call.
	fn schedule(&self) -> &Schedule<Self::T>;

	/// Information needed for rent calculations.
	fn rent_params(&self) -> &RentParams<Self::T>;

	/// Get a mutable reference to the nested gas meter.
	fn gas_meter(&mut self) -> &mut GasMeter<Self::T>;
}

/// Describes the different functions that can be exported by an [`Executable`].
#[derive(Clone, Copy, PartialEq)]
pub enum ExportedFunction {
	/// The constructor function which is executed on deployment of a contract.
	Constructor,
	/// The function which is executed when a contract is called.
	Call,
}

/// A trait that represents something that can be executed.
///
/// In the on-chain environment this would be represented by a wasm module. This trait exists in
/// order to be able to mock the wasm logic for testing.
pub trait Executable<T: Config>: Sized {
	/// Load the executable from storage.
	fn from_storage(
		code_hash: CodeHash<T>,
		schedule: &Schedule<T>,
		gas_meter: &mut GasMeter<T>,
	) -> Result<Self, DispatchError>;

	/// Load the module from storage without re-instrumenting it.
	///
	/// A code module is re-instrumented on-load when it was originally instrumented with
	/// an older schedule. This skips this step for cases where the code storage is
	/// queried for purposes other than execution.
	fn from_storage_noinstr(code_hash: CodeHash<T>) -> Result<Self, DispatchError>;

	/// Decrements the refcount by one and deletes the code if it drops to zero.
	fn drop_from_storage(self);

	/// Increment the refcount by one. Fails if the code does not exist on-chain.
	///
	/// Returns the size of the original code.
	fn add_user(code_hash: CodeHash<T>) -> Result<u32, DispatchError>;

	/// Decrement the refcount by one and remove the code when it drops to zero.
	///
	/// Returns the size of the original code.
	fn remove_user(code_hash: CodeHash<T>) -> u32;

	/// Execute the specified exported function and return the result.
	///
	/// When the specified function is `Constructor` the executable is stored and its
	/// refcount incremented.
	///
	/// # Note
	///
	/// This functions expects to be executed in a storage transaction that rolls back
	/// all of its emitted storage changes.
	fn execute<E: Ext<T = T>>(
		self,
		ext: &mut E,
		function: &ExportedFunction,
		input_data: Vec<u8>,
	) -> ExecResult;

	/// The code hash of the executable.
	fn code_hash(&self) -> &CodeHash<T>;

	/// Size of the instrumented code in bytes.
	fn code_len(&self) -> u32;

	/// Sum of instrumented and pristine code len.
	fn aggregate_code_len(&self) -> u32;

	// The number of contracts using this executable.
	fn refcount(&self) -> u32;

	/// The storage that is occupied by the instrumented executable and its pristine source.
	///
	/// The returned size is already divided by the number of users who share the code.
	/// This is essentially `aggregate_code_len() / refcount()`.
	///
	/// # Note
	///
	/// This works with the current in-memory value of refcount. When calling any contract
	/// without refetching this from storage the result can be inaccurate as it might be
	/// working with a stale value. Usually this inaccuracy is tolerable.
	fn occupied_storage(&self) -> u32 {
		// We disregard the size of the struct itself as the size is completely
		// dominated by the code size.
		let len = self.aggregate_code_len();
		len.checked_div(self.refcount()).unwrap_or(len)
	}
}

pub struct Stack<'a, T: Config, E> {
	origin: T::AccountId,
	schedule: &'a Schedule<T>,
	gas_meter: &'a mut GasMeter<T>,
	timestamp: MomentOf<T>,
	block_number: T::BlockNumber,
	account_counter: Option<u64>,
	first_frame: Frame<T>,
	frames: Vec<Frame<T>>,
	_phantom: PhantomData<E>,
}

enum CachedContract<T: Config> {
	Cached(AliveContractInfo<T>),
	Invalidated,
	Terminated,
}

macro_rules! get_cached_or_panic {
	($c:expr) => {{
		if let CachedContract::Cached(contract) = $c {
			contract
		} else {
			panic!(
				"It is impossible to remove a contract that is on the call stack;\
				See implementations of terminate and restore_to;\
				Therefore fetching a contract will never fail while using an account id
				that is currently active on the call stack;\
				qed"
			);
		}
	}}
}

impl<T: Config> CachedContract<T> {
	fn load(&mut self, account_id: &T::AccountId) {
		if let CachedContract::Invalidated = self {
			let contract = <ContractInfoOf<T>>::get(&account_id)
				.and_then(|contract| contract.get_alive());
			if let Some(contract) = contract {
				*self = CachedContract::Cached(contract);
			}
		}
	}

	fn as_alive(&mut self, account_id: &T::AccountId) -> &mut AliveContractInfo<T> {
		self.load(account_id);
		get_cached_or_panic!(self)
	}

	fn invalidate(&mut self, account_id: &T::AccountId) -> AliveContractInfo<T> {
		self.load(account_id);
		get_cached_or_panic!(mem::replace(self, Self::Invalidated))
	}

	fn terminate(&mut self, account_id: &T::AccountId) -> AliveContractInfo<T> {
		self.load(account_id);
		get_cached_or_panic!(mem::replace(self, Self::Terminated))
	}
}

struct Frame<T: Config> {
	account_id: T::AccountId,
	contract_info: CachedContract<T>,
	value_transferred: BalanceOf<T>,
	rent_params: RentParams<T>,
	entry_point: ExportedFunction,
	nested_meter: GasMeter<T>,
}

impl<T: Config> Frame<T> {
	fn contract_info(&mut self) -> &mut AliveContractInfo<T> {
		self.contract_info.as_alive(&self.account_id)
	}

	fn invalidate(&mut self) -> AliveContractInfo<T> {
		self.contract_info.invalidate(&self.account_id)
	}

	fn terminate(&mut self) -> AliveContractInfo<T> {
		self.contract_info.terminate(&self.account_id)
	}
}

enum FrameArgs<'a, T: Config, E> {
	Call(T::AccountId, Option<AliveContractInfo<T>>),
	Instantiate(T::AccountId, u64, E, &'a [u8]),
}

impl<'a, T, E> Stack<'a, T, E>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
	E: Executable<T>,
{
	/// Make a call to the specified address, optionally transferring some funds.
	///
	/// # Return Value
	///
	/// Result<(ExecReturnValue, CodeSize), (ExecError, CodeSize)>
	pub fn with_call(
		origin: T::AccountId,
		dest: T::AccountId,
		gas_meter: &'a mut GasMeter<T>,
		schedule: &'a Schedule<T>,
		value: BalanceOf<T>,
		input_data: Vec<u8>,
	) -> Result<(ExecReturnValue, u32), (ExecError, u32)> {
		let (mut stack, executable) = Self::new(
			FrameArgs::Call(dest, None),
			origin,
			gas_meter,
			schedule,
			value,
		)?;
		stack.run(executable, input_data)
	}

	pub fn with_instantiate(
		origin: T::AccountId,
		executable: E,
		gas_meter: &'a mut GasMeter<T>,
		schedule: &'a Schedule<T>,
		value: BalanceOf<T>,
		input_data: Vec<u8>,
		salt: &[u8],
	) -> Result<(T::AccountId, ExecReturnValue), ExecError> {
		let (mut stack, executable) = Self::new(
			FrameArgs::Instantiate(
				origin.clone(), Self::initial_account_seed(), executable, salt,
			),
			origin,
			gas_meter,
			schedule,
			value,
		).map_err(|(e, _code_len)| e)?;
		let account_id = stack.frame().account_id.clone();
		stack.run(executable, input_data)
			.map(|(ret, _code_len)| (account_id, ret))
			.map_err(|(err, _code_len)| err)
	}

	fn new(
		args: FrameArgs<T, E>,
		origin: T::AccountId,
		gas_meter: &'a mut GasMeter<T>,
		schedule: &'a Schedule<T>,
		value: BalanceOf<T>,
	) -> Result<(Self, E), (ExecError, u32)> {
		let (first_frame, executable) = Self::new_frame(args, value, gas_meter, 0, &schedule)?;
		let stack = Self {
			origin,
			schedule,
			gas_meter,
			timestamp: T::Time::now(),
			block_number: <frame_system::Pallet<T>>::block_number(),
			account_counter: None,
			first_frame,
			frames: Vec::new(),
			_phantom: Default::default(),
		};
		Ok((stack, executable))
	}

	fn new_frame(
		frame_args: FrameArgs<T, E>,
		value_transferred: BalanceOf<T>,
		gas_meter: &mut GasMeter<T>,
		gas_limit: Weight,
		schedule: &Schedule<T>
	) -> Result<(Frame<T>, E), (ExecError, u32)> {
		if T::MaxDepth::get() == 0 {
			return Err((Error::<T>::MaxCallDepthReached.into(), 0));
		}

		let (account_id, contract_info, executable, entry_point) = match frame_args {
			FrameArgs::Call(account_id, contract) => {
				let contract = if let Some(contract) = contract {
					contract
				} else {
					<ContractInfoOf<T>>::get(&account_id)
						.and_then(|contract| contract.get_alive())
						.ok_or((Error::<T>::NotCallable.into(), 0))?
				};

				let executable = E::from_storage(contract.code_hash, schedule, gas_meter)
					.map_err(|e| (e.into(), 0))?;

				// This charges the rent and denies access to a contract that is in need of
				// eviction by returning `None`. We cannot evict eagerly here because those
				// changes would be rolled back in case this contract is called by another
				// contract.
				// See: https://github.com/paritytech/substrate/issues/6439#issuecomment-648754324
				let contract = Rent::<T, E>
					::charge(&account_id, contract, executable.occupied_storage())
					.map_err(|e| (e.into(), executable.code_len()))?
					.ok_or((Error::<T>::NotCallable.into(), executable.code_len()))?;
				(account_id, contract, executable, ExportedFunction::Call)
			}
			FrameArgs::Instantiate(caller, seed, executable, salt) => {
				let account_id = <Contracts<T>>::contract_address(
					&caller, executable.code_hash(), &salt,
				);
				let trie_id = Storage::<T>::generate_trie_id(&account_id, seed);
				let contract = Storage::<T>::new_contract(
					&account_id,
					trie_id,
					executable.code_hash().clone(),
				).map_err(|e| (e.into(), executable.code_len()))?;
				(account_id, contract, executable, ExportedFunction::Constructor)
			}
		};

		let frame = Frame {
			rent_params: RentParams::new(&account_id, &contract_info, &executable),
			value_transferred,
			contract_info: CachedContract::Cached(contract_info),
			account_id,
			entry_point,
			nested_meter: gas_meter.nested(gas_limit)
				.map_err(|e| (e.into(), executable.code_len()))?,
		};

		Ok((frame, executable))
	}

	fn push_frame(
		&mut self,
		frame_args: FrameArgs<T, E>,
		value_transferred: BalanceOf<T>,
		gas_limit: Weight,
	) -> Result<E, (ExecError, u32)> {
		if self.depth() == T::MaxDepth::get() {
			return Err((Error::<T>::MaxCallDepthReached.into(), 0));
		}
		let (frame, executable) = Self::new_frame(
			frame_args,
			value_transferred,
			self.gas_meter,
			gas_limit,
			self.schedule,
		)?;
		self.frames.push(frame);
		Ok(executable)
	}

	fn run(
		&mut self,
		executable: E,
		input_data: Vec<u8>
	) -> Result<(ExecReturnValue, u32), (ExecError, u32)> {
		let output = self.raw_run(executable, input_data);
		if !output.is_ok() && self.frame().entry_point == ExportedFunction::Constructor {
			self.account_counter.as_mut().map(|c| *c = c.wrapping_sub(1));
		}
		self.pop_frame(output.is_ok());
		output
	}

	fn raw_run(
		&mut self,
		executable: E,
		input_data: Vec<u8>
	) -> Result<(ExecReturnValue, u32), (ExecError, u32)> {
		// Cache the value before calling into the constructor because that
		// consumes the value. If the constructor creates additional contracts using
		// the same code hash we still charge the "1 block rent" as if they weren't
		// spawned. This is OK as overcharging is always safe.
		let occupied_storage = executable.occupied_storage();
		let code_len = executable.code_len();
		let entry_point = self.frame().entry_point;

		let output = with_transaction(|| {
			let output = self.initial_transfer().map_err(|e| (ExecError::from(e), 0));
			if let Err(err) = output {
				return TransactionOutcome::Rollback(Err(err))
			}

			let output = executable.execute(
				self,
				&entry_point,
				input_data,
			).map_err(|e| (ExecError { error: e.error, origin: ErrorOrigin::Callee }, code_len));

			match output {
				Ok(_) => TransactionOutcome::Commit(output),
				Err(_) => TransactionOutcome::Rollback(output),
			}
		});

		if output.is_ok() && entry_point == ExportedFunction::Constructor {
			let frame = self.frame_mut();
			let account_id = frame.account_id.clone();

			// It is not allowed to terminate a contract inside its constructor
			if let CachedContract::Terminated = frame.contract_info {
				return Err((Error::<T>::NotCallable.into(), code_len));
			}

			// Collect the rent for the first block to prevent the creation of very large
			// contracts that never intended to pay for even one block.
			// This also makes sure that it is above the subsistence threshold
			// in order to keep up the guarantuee that we always leave a tombstone behind
			// with the exception of a contract that called `seal_terminate`.
			let contract = Rent::<T, E>::charge(&account_id, frame.invalidate(), occupied_storage)
				.map_err(|e| (e.into(), code_len))?
				.ok_or((Error::<T>::NewContractNotFunded.into(), code_len))?;
			frame.contract_info = CachedContract::Cached(contract);

			// Deposit an instantiation event.
			deposit_event::<T>(vec![], Event::Instantiated(
				self.caller().clone(),
				account_id,
			));
		}

		Ok((output?, code_len))
	}

	/// Transfer some funds from `transactor` to `dest`.
	///
	/// We only allow allow for draining all funds of the sender if `cause` is
	/// is specified as `Terminate`. Otherwise, any transfer that would bring the sender below the
	/// subsistence threshold (for contracts) or the existential deposit (for plain accounts)
	/// results in an error.
	fn transfer(
		sender_is_contract: bool,
		allow_death: bool,
		from: &T::AccountId,
		to: &T::AccountId,
		value: BalanceOf<T>,
	) -> DispatchResult {
		if value == 0u32.into() {
			return Ok(());
		}

		// Only seal_terminate is allowed to bring the sender below the subsistence
		// threshold or even existential deposit.
		let existence_requirement = match (allow_death, sender_is_contract) {
			(true, _) => ExistenceRequirement::AllowDeath,
			(false, true) => {
				ensure!(
					T::Currency::total_balance(from).saturating_sub(value) >=
						Contracts::<T>::subsistence_threshold(),
					Error::<T>::BelowSubsistenceThreshold,
				);
				ExistenceRequirement::KeepAlive
			},
			(false, false) => ExistenceRequirement::KeepAlive,
		};

		T::Currency::transfer(from, to, value, existence_requirement)
			.map_err(|_| Error::<T>::TransferFailed)?;

		Ok(())
	}

	fn initial_transfer(&self) -> DispatchResult {
		Self::transfer(
			self.caller_is_contract(),
			false,
			self.caller(),
			&self.frame().account_id,
			self.frame().value_transferred,
		)
	}

	fn depth(&self) -> u32 {
		(self.frames.len() + 1) as u32
	}

	fn caller_is_contract(&self) -> bool {
		self.depth() > 1
	}

	fn pop_frame(&mut self, persist: bool) {
		// Pop the current frame from the stack and return it in case it needs to interact
		// with duplicates that might exist on the stack,.
		let (account_id, contract) = {
			let frame = self.frames.pop();
			if !persist {
				return;
			}
			if let Some(frame) = frame {
				if let CachedContract::Cached(contract) = frame.contract_info {
					(frame.account_id, contract)
				} else {
					return;
				}
			} else {
				// Only the first frame exists: Just write it to storage and return
				if let CachedContract::Cached(contract) = &self.first_frame.contract_info {
					<ContractInfoOf<T>>::insert(
						&self.first_frame.account_id,
						ContractInfo::Alive(contract.clone())
					);
				}
				return;
			}
		};

		// optimization: Predecessor is the same contract.
		let prev = self.frame_mut();
		if prev.account_id == account_id {
			prev.contract_info = CachedContract::Cached(contract);
			return;
		}

		// Invalidate stale data: Only the first contract needs to be invalidated.
		// Other duplicates are invalidated when their childs are popped from the stack.
		if let Some(same) = self.frames_mut().skip(1).find(|f| f.account_id == account_id) {
			same.contract_info = CachedContract::Invalidated;
		}

		// It is OK to store it here because active references to it are invalidated.
		<ContractInfoOf<T>>::insert(&account_id, ContractInfo::Alive(contract));
	}

	fn frame(&self) -> &Frame<T> {
		self.frames.last().unwrap_or(&self.first_frame)
	}

	fn frame_mut(&mut self) -> &mut Frame<T> {
		self.frames.last_mut().unwrap_or(&mut self.first_frame)
	}

	fn frames(&self) -> impl Iterator<Item=&Frame<T>> {
		sp_std::iter::once(&self.first_frame)
			.chain(&self.frames)
			.rev()
	}

	fn frames_mut(&mut self) -> impl Iterator<Item=&mut Frame<T>> {
		sp_std::iter::once(&mut self.first_frame)
			.chain(&mut self.frames)
			.rev()
	}

	/// Returns whether the current contract is on the stack multiple times.
	fn is_recursive(&self) -> bool {
		let account_id = &self.frame().account_id;
		self.frames().skip(1).any(|f| &f.account_id == account_id)
	}

	fn next_account_seed(&mut self) -> u64 {
		let next = if let Some(current) = self.account_counter {
			current + 1
		} else {
			Self::initial_account_seed()
		};
		self.account_counter = Some(next);
		next
	}

	fn initial_account_seed() -> u64 {
		<AccountCounter<T>>::get().wrapping_add(1)
	}
}

impl<'a, T, E> Ext for Stack<'a, T, E>
where
	T: Config,
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>,
	E: Executable<T>,
{
	type T = T;

	fn call(
		&mut self,
		gas_limit: Weight,
		to: T::AccountId,
		value: BalanceOf<T>,
		input_data: Vec<u8>,
	) -> Result<(ExecReturnValue, u32), (ExecError, u32)> {
		let existing = self
			.frames()
			.filter(|f| f.entry_point == ExportedFunction::Call)
			.find(|f| f.account_id == to).and_then(|f| {
				match &f.contract_info {
					CachedContract::Cached(contract) => Some(contract.clone()),
					_ => None,
				}
			});
		let executable = self.push_frame(FrameArgs::Call(to, existing), value, gas_limit)?;
		self.run(executable, input_data)
	}

	fn instantiate(
		&mut self,
		gas_limit: Weight,
		code_hash: CodeHash<T>,
		endowment: BalanceOf<T>,
		input_data: Vec<u8>,
		salt: &[u8],
	) -> Result<(AccountIdOf<T>, ExecReturnValue, u32), (ExecError, u32)> {
		let executable = E::from_storage(code_hash, &self.schedule, self.gas_meter)
			.map_err(|e| (e.into(), 0))?;
		let seed = self.next_account_seed();
		let executable = self.push_frame(
			FrameArgs::Instantiate(self.frame().account_id.clone(), seed, executable, salt),
			endowment,
			gas_limit,
		)?;
		let account_id = self.frame().account_id.clone();
		self.run(executable, input_data)
			.map(|(ret, code_len)| (account_id, ret, code_len))
	}

	fn terminate(
		&mut self,
		beneficiary: &AccountIdOf<Self::T>,
	) -> Result<u32, (DispatchError, u32)> {
		if self.is_recursive() {
			return Err((Error::<T>::ReentranceDenied.into(), 0));
		}
		let frame = self.frame_mut();
		let info = frame.terminate();
		Storage::<T>::queue_trie_for_deletion(&info).map_err(|e| (e, 0))?;
		<Stack<'a, T, E>>::transfer(
			true,
			true,
			&frame.account_id,
			beneficiary,
			T::Currency::free_balance(&frame.account_id),
		).map_err(|e| (e, 0))?;
		ContractInfoOf::<T>::remove(&frame.account_id);
		let code_len = E::remove_user(info.code_hash);
		Contracts::<T>::deposit_event(
			Event::Terminated(frame.account_id.clone(), beneficiary.clone()),
		);
		Ok(code_len)
	}

	fn restore_to(
		&mut self,
		dest: AccountIdOf<Self::T>,
		code_hash: CodeHash<Self::T>,
		rent_allowance: BalanceOf<Self::T>,
		delta: Vec<StorageKey>,
	) -> Result<(u32, u32), (DispatchError, u32, u32)> {
		if self.is_recursive() {
			return Err((Error::<T>::ReentranceDenied.into(), 0, 0));
		}
		let result = Rent::<T, E>::restore_to(
			self.frame().account_id.clone(),
			dest.clone(),
			code_hash.clone(),
			rent_allowance,
			delta,
		);
		if let Ok(_) = result {
			deposit_event::<Self::T>(
				vec![],
				Event::Restored(
					self.frame().account_id.clone(),
					dest,
					code_hash,
					rent_allowance,
				),
			);
		}
		result
	}

	fn transfer(
		&mut self,
		to: &T::AccountId,
		value: BalanceOf<T>,
	) -> DispatchResult {
		Self::transfer(true, false, &self.frame().account_id, to, value)
	}

	fn get_storage(&mut self, key: &StorageKey) -> Option<Vec<u8>> {
		Storage::<T>::read(&self.frame_mut().contract_info().trie_id, key)
	}

	fn set_storage(&mut self, key: StorageKey, value: Option<Vec<u8>>) -> DispatchResult {
		let block_number = self.block_number;
		let frame = self.frame_mut();
		Storage::<T>::write(
			block_number, frame.contract_info(), &key, value,
		)
	}

	fn address(&self) -> &T::AccountId {
		&self.frame().account_id
	}

	fn caller(&self) -> &T::AccountId {
		self.frames().nth(1).map(|f| &f.account_id).unwrap_or(&self.origin)
	}

	fn balance(&self) -> BalanceOf<T> {
		T::Currency::free_balance(&self.frame().account_id)
	}

	fn value_transferred(&self) -> BalanceOf<T> {
		self.frame().value_transferred
	}

	fn random(&self, subject: &[u8]) -> (SeedOf<T>, BlockNumberOf<T>) {
		T::Randomness::random(subject)
	}

	fn now(&self) -> &MomentOf<T> {
		&self.timestamp
	}

	fn minimum_balance(&self) -> BalanceOf<T> {
		T::Currency::minimum_balance()
	}

	fn tombstone_deposit(&self) -> BalanceOf<T> {
		T::TombstoneDeposit::get()
	}

	fn deposit_event(&mut self, topics: Vec<T::Hash>, data: Vec<u8>) {
		deposit_event::<Self::T>(
			topics,
			Event::ContractEmitted(self.frame().account_id.clone(), data)
		);
	}

	fn set_rent_allowance(&mut self, rent_allowance: BalanceOf<T>) {
		self.frame_mut().contract_info().rent_allowance = rent_allowance;
	}

	fn rent_allowance(&mut self) -> BalanceOf<T> {
		self.frame_mut().contract_info().rent_allowance
	}

	fn block_number(&self) -> T::BlockNumber { self.block_number }

	fn max_value_size(&self) -> u32 {
		T::MaxValueSize::get()
	}

	fn get_weight_price(&self, weight: Weight) -> BalanceOf<Self::T> {
		T::WeightPrice::convert(weight)
	}

	fn schedule(&self) -> &Schedule<Self::T> {
		&self.schedule
	}

	fn rent_params(&self) -> &RentParams<Self::T> {
		&self.frame().rent_params
	}

	fn gas_meter(&mut self) -> &mut GasMeter<Self::T> {
		&mut self.frame_mut().nested_meter
	}
}

fn deposit_event<T: Config>(
	topics: Vec<T::Hash>,
	event: Event<T>,
) {
	<frame_system::Pallet<T>>::deposit_event_indexed(
		&*topics,
		<T as Config>::Event::from(event).into(),
	)
}

mod sealing {
	use super::*;

	pub trait Sealed {}

	impl<'a, T: Config, E> Sealed for Stack<'a, T, E> {}

	#[cfg(test)]
	impl Sealed for crate::wasm::MockExt {}

	#[cfg(test)]
	impl Sealed for &mut crate::wasm::MockExt {}
}

/// These tests exercise the executive layer.
///
/// In these tests the VM/loader are mocked. Instead of dealing with wasm bytecode they use simple closures.
/// This allows you to tackle executive logic more thoroughly without writing a
/// wasm VM code.
#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		gas::GasMeter, tests::{ExtBuilder, Test, Event as MetaEvent},
		storage::Storage,
		tests::{
			ALICE, BOB, CHARLIE,
			test_utils::{place_contract, set_balance, get_balance},
		},
		exec::ExportedFunction::*,
		Error, Weight, CurrentSchedule,
	};
	use sp_runtime::DispatchError;
	use assert_matches::assert_matches;
	use std::{cell::RefCell, collections::HashMap, rc::Rc};
	use pretty_assertions::{assert_eq, assert_ne};

	type MockStack<'a> = Stack<'a, Test, MockExecutable>;

	const GAS_LIMIT: Weight = 10_000_000_000;

	thread_local! {
		static LOADER: RefCell<MockLoader> = RefCell::new(MockLoader::default());
	}

	fn events() -> Vec<Event<Test>> {
		<frame_system::Pallet<Test>>::events()
			.into_iter()
			.filter_map(|meta| match meta.event {
				MetaEvent::pallet_contracts(contract_event) => Some(contract_event),
				_ => None,
			})
			.collect()
	}

	struct MockCtx<'a> {
		ext: &'a mut dyn Ext<T = Test>,
		input_data: Vec<u8>,
		gas_meter: &'a mut GasMeter<Test>,
	}

	#[derive(Clone)]
	struct MockExecutable {
		func: Rc<dyn Fn(MockCtx, &Self) -> ExecResult + 'static>,
		func_type: ExportedFunction,
		code_hash: CodeHash<Test>,
		refcount: u64,
	}

	#[derive(Default)]
	struct MockLoader {
		map: HashMap<CodeHash<Test>, MockExecutable>,
		counter: u64,
	}

	impl MockLoader {
		fn insert(
			func_type: ExportedFunction,
			f: impl Fn(MockCtx, &MockExecutable,
		) -> ExecResult + 'static) -> CodeHash<Test> {
			LOADER.with(|loader| {
				let mut loader = loader.borrow_mut();
				// Generate code hashes as monotonically increasing values.
				let hash = <Test as frame_system::Config>::Hash::from_low_u64_be(loader.counter);
				loader.counter += 1;
				loader.map.insert(hash, MockExecutable {
					func: Rc::new(f),
					func_type,
					code_hash: hash.clone(),
					refcount: 1,
				});
				hash
			})
		}

		fn increment_refcount(code_hash: CodeHash<Test>) {
			LOADER.with(|loader| {
				let mut loader = loader.borrow_mut();
				loader.map
					.entry(code_hash)
					.and_modify(|executable| executable.refcount += 1)
					.or_insert_with(|| panic!("code_hash does not exist"));
			});
		}

		fn decrement_refcount(code_hash: CodeHash<Test>) {
			use std::collections::hash_map::Entry::Occupied;
			LOADER.with(|loader| {
				let mut loader = loader.borrow_mut();
				let mut entry = match loader.map.entry(code_hash) {
					Occupied(e) => e,
					_ => panic!("code_hash does not exist"),
				};
				let refcount = &mut entry.get_mut().refcount;
				*refcount -= 1;
				if *refcount == 0 {
					entry.remove();
				}
			});
		}

		fn refcount(code_hash: &CodeHash<Test>) -> u32 {
			LOADER.with(|loader| {
				loader
					.borrow()
					.map
					.get(code_hash)
					.expect("code_hash does not exist")
					.refcount()
			})
		}
	}

	impl Executable<Test> for MockExecutable {
		fn from_storage(
			code_hash: CodeHash<Test>,
			_schedule: &Schedule<Test>,
			_gas_meter: &mut GasMeter<Test>,
		) -> Result<Self, DispatchError> {
			Self::from_storage_noinstr(code_hash)
		}

		fn from_storage_noinstr(code_hash: CodeHash<Test>) -> Result<Self, DispatchError> {
			LOADER.with(|loader| {
				loader.borrow_mut()
					.map
					.get(&code_hash)
					.cloned()
					.ok_or(Error::<Test>::CodeNotFound.into())
			})
		}

		fn drop_from_storage(self) {
			MockLoader::decrement_refcount(self.code_hash);
		}

		fn add_user(code_hash: CodeHash<Test>) -> Result<u32, DispatchError> {
			MockLoader::increment_refcount(code_hash);
			Ok(0)
		}

		fn remove_user(code_hash: CodeHash<Test>) -> u32 {
			MockLoader::decrement_refcount(code_hash);
			0
		}

		fn execute<E: Ext<T = Test>>(
			self,
			mut ext: E,
			function: &ExportedFunction,
			input_data: Vec<u8>,
			gas_meter: &mut GasMeter<Test>,
		) -> ExecResult {
			if let &Constructor = function {
				MockLoader::increment_refcount(self.code_hash);
			}
			if function == &self.func_type {
				(self.func)(MockCtx {
					ext: &mut ext,
					input_data,
					gas_meter,
				}, &self)
			} else {
				exec_success()
			}
		}

		fn code_hash(&self) -> &CodeHash<Test> {
			&self.code_hash
		}

		fn code_len(&self) -> u32 {
			0
		}

		fn aggregate_code_len(&self) -> u32 {
			0
		}

		fn refcount(&self) -> u32 {
			self.refcount as u32
		}
	}

	fn exec_success() -> ExecResult {
		Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: Vec::new() })
	}

	#[test]
	fn it_works() {
		thread_local! {
			static TEST_DATA: RefCell<Vec<usize>> = RefCell::new(vec![0]);
		}

		let value = Default::default();
		let mut gas_meter = GasMeter::<Test>::new(GAS_LIMIT);
		let exec_ch = MockLoader::insert(Call, |_ctx, _executable| {
			TEST_DATA.with(|data| data.borrow_mut().push(1));
			exec_success()
		});

		ExtBuilder::default().build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			place_contract(&BOB, exec_ch);

			assert_matches!(
				ctx.call(BOB, value, &mut gas_meter, vec![]),
				Ok(_)
			);
		});

		TEST_DATA.with(|data| assert_eq!(*data.borrow(), vec![0, 1]));
	}

	#[test]
	fn transfer_works() {
		// This test verifies that a contract is able to transfer
		// some funds to another account.
		let origin = ALICE;
		let dest = BOB;

		ExtBuilder::default().build().execute_with(|| {
			set_balance(&origin, 100);
			set_balance(&dest, 0);

			super::transfer::<Test>(
				super::TransferCause::Call,
				super::TransactorKind::PlainAccount,
				&origin,
				&dest,
				55,
			).unwrap();

			assert_eq!(get_balance(&origin), 45);
			assert_eq!(get_balance(&dest), 55);
		});
	}

	#[test]
	fn changes_are_reverted_on_failing_call() {
		// This test verifies that changes are reverted on a call which fails (or equally, returns
		// a non-zero status code).
		let origin = ALICE;
		let dest = BOB;

		let return_ch = MockLoader::insert(
			Call,
			|_, _| Ok(ExecReturnValue { flags: ReturnFlags::REVERT, data: Vec::new() })
		);

		ExtBuilder::default().build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(origin.clone(), &schedule);
			place_contract(&BOB, return_ch);
			set_balance(&origin, 100);
			let balance = get_balance(&dest);

			let output = ctx.call(
				dest.clone(),
				55,
				&mut GasMeter::<Test>::new(GAS_LIMIT),
				vec![],
			).unwrap();

			assert!(!output.0.is_success());
			assert_eq!(get_balance(&origin), 100);

			// the rent is still charged
			assert!(get_balance(&dest) < balance);
		});
	}

	#[test]
	fn balance_too_low() {
		// This test verifies that a contract can't send value if it's
		// balance is too low.
		let origin = ALICE;
		let dest = BOB;

		ExtBuilder::default().build().execute_with(|| {
			set_balance(&origin, 0);

			let result = super::transfer::<Test>(
				super::TransferCause::Call,
				super::TransactorKind::PlainAccount,
				&origin,
				&dest,
				100,
			);

			assert_eq!(
				result,
				Err(Error::<Test>::TransferFailed.into())
			);
			assert_eq!(get_balance(&origin), 0);
			assert_eq!(get_balance(&dest), 0);
		});
	}

	#[test]
	fn output_is_returned_on_success() {
		// Verifies that if a contract returns data with a successful exit status, this data
		// is returned from the execution context.
		let origin = ALICE;
		let dest = BOB;
		let return_ch = MockLoader::insert(
			Call,
			|_, _| Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: vec![1, 2, 3, 4] })
		);

		ExtBuilder::default().build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(origin, &schedule);
			place_contract(&BOB, return_ch);

			let result = ctx.call(
				dest,
				0,
				&mut GasMeter::<Test>::new(GAS_LIMIT),
				vec![],
			);

			let output = result.unwrap();
			assert!(output.0.is_success());
			assert_eq!(output.0.data, vec![1, 2, 3, 4]);
		});
	}

	#[test]
	fn output_is_returned_on_failure() {
		// Verifies that if a contract returns data with a failing exit status, this data
		// is returned from the execution context.
		let origin = ALICE;
		let dest = BOB;
		let return_ch = MockLoader::insert(
			Call,
			|_, _| Ok(ExecReturnValue { flags: ReturnFlags::REVERT, data: vec![1, 2, 3, 4] })
		);

		ExtBuilder::default().build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(origin, &schedule);
			place_contract(&BOB, return_ch);

			let result = ctx.call(
				dest,
				0,
				&mut GasMeter::<Test>::new(GAS_LIMIT),
				vec![],
			);

			let output = result.unwrap();
			assert!(!output.0.is_success());
			assert_eq!(output.0.data, vec![1, 2, 3, 4]);
		});
	}

	#[test]
	fn input_data_to_call() {
		let input_data_ch = MockLoader::insert(Call, |ctx, _| {
			assert_eq!(ctx.input_data, &[1, 2, 3, 4]);
			exec_success()
		});

		// This one tests passing the input data into a contract via call.
		ExtBuilder::default().build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			place_contract(&BOB, input_data_ch);

			let result = ctx.call(
				BOB,
				0,
				&mut GasMeter::<Test>::new(GAS_LIMIT),
				vec![1, 2, 3, 4],
			);
			assert_matches!(result, Ok(_));
		});
	}

	#[test]
	fn input_data_to_instantiate() {
		let input_data_ch = MockLoader::insert(Constructor, |ctx, _| {
			assert_eq!(ctx.input_data, &[1, 2, 3, 4]);
			exec_success()
		});

		// This one tests passing the input data into a contract via instantiate.
		ExtBuilder::default().build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let subsistence = Contracts::<Test>::subsistence_threshold();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			let mut gas_meter = GasMeter::<Test>::new(GAS_LIMIT);
			let executable = MockExecutable::from_storage(
				input_data_ch, &schedule, &mut gas_meter
			).unwrap();

			set_balance(&ALICE, subsistence * 10);

			let result = ctx.instantiate(
				subsistence * 3,
				&mut gas_meter,
				executable,
				vec![1, 2, 3, 4],
				&[],
			);
			assert_matches!(result, Ok(_));
		});
	}

	#[test]
	fn max_depth() {
		// This test verifies that when we reach the maximal depth creation of an
		// yet another context fails.
		thread_local! {
			static REACHED_BOTTOM: RefCell<bool> = RefCell::new(false);
		}
		let value = Default::default();
		let recurse_ch = MockLoader::insert(Call, |ctx, _| {
			// Try to call into yourself.
			let r = ctx.ext.call(&BOB, 0, ctx.gas_meter, vec![]);

			REACHED_BOTTOM.with(|reached_bottom| {
				let mut reached_bottom = reached_bottom.borrow_mut();
				if !*reached_bottom {
					// We are first time here, it means we just reached bottom.
					// Verify that we've got proper error and set `reached_bottom`.
					assert_eq!(
						r,
						Err((Error::<Test>::MaxCallDepthReached.into(), 0))
					);
					*reached_bottom = true;
				} else {
					// We just unwinding stack here.
					assert_matches!(r, Ok(_));
				}
			});

			exec_success()
		});

		ExtBuilder::default().build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			set_balance(&BOB, 1);
			place_contract(&BOB, recurse_ch);

			let result = ctx.call(
				BOB,
				value,
				&mut GasMeter::<Test>::new(GAS_LIMIT),
				vec![],
			);

			assert_matches!(result, Ok(_));
		});
	}

	#[test]
	fn caller_returns_proper_values() {
		let origin = ALICE;
		let dest = BOB;

		thread_local! {
			static WITNESSED_CALLER_BOB: RefCell<Option<AccountIdOf<Test>>> = RefCell::new(None);
			static WITNESSED_CALLER_CHARLIE: RefCell<Option<AccountIdOf<Test>>> = RefCell::new(None);
		}

		let bob_ch = MockLoader::insert(Call, |ctx, _| {
			// Record the caller for bob.
			WITNESSED_CALLER_BOB.with(|caller|
				*caller.borrow_mut() = Some(ctx.ext.caller().clone())
			);

			// Call into CHARLIE contract.
			assert_matches!(
				ctx.ext.call(&CHARLIE, 0, ctx.gas_meter, vec![]),
				Ok(_)
			);
			exec_success()
		});
		let charlie_ch = MockLoader::insert(Call, |ctx, _| {
			// Record the caller for charlie.
			WITNESSED_CALLER_CHARLIE.with(|caller|
				*caller.borrow_mut() = Some(ctx.ext.caller().clone())
			);
			exec_success()
		});

		ExtBuilder::default().build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(origin.clone(), &schedule);
			place_contract(&dest, bob_ch);
			place_contract(&CHARLIE, charlie_ch);

			let result = ctx.call(
				dest.clone(),
				0,
				&mut GasMeter::<Test>::new(GAS_LIMIT),
				vec![],
			);

			assert_matches!(result, Ok(_));
		});

		WITNESSED_CALLER_BOB.with(|caller| assert_eq!(*caller.borrow(), Some(origin)));
		WITNESSED_CALLER_CHARLIE.with(|caller| assert_eq!(*caller.borrow(), Some(dest)));
	}

	#[test]
	fn address_returns_proper_values() {
		let bob_ch = MockLoader::insert(Call, |ctx, _| {
			// Verify that address matches BOB.
			assert_eq!(*ctx.ext.address(), BOB);

			// Call into charlie contract.
			assert_matches!(
				ctx.ext.call(&CHARLIE, 0, ctx.gas_meter, vec![]),
				Ok(_)
			);
			exec_success()
		});
		let charlie_ch = MockLoader::insert(Call, |ctx, _| {
			assert_eq!(*ctx.ext.address(), CHARLIE);
			exec_success()
		});

		ExtBuilder::default().build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			place_contract(&BOB, bob_ch);
			place_contract(&CHARLIE, charlie_ch);

			let result = ctx.call(
				BOB,
				0,
				&mut GasMeter::<Test>::new(GAS_LIMIT),
				vec![],
			);

			assert_matches!(result, Ok(_));
		});
	}

	#[test]
	fn refuse_instantiate_with_value_below_existential_deposit() {
		let dummy_ch = MockLoader::insert(Constructor, |_, _| exec_success());

		ExtBuilder::default().existential_deposit(15).build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			let mut gas_meter = GasMeter::<Test>::new(GAS_LIMIT);
			let executable = MockExecutable::from_storage(
				dummy_ch, &schedule, &mut gas_meter
			).unwrap();

			assert_matches!(
				ctx.instantiate(
					0, // <- zero endowment
					&mut gas_meter,
					executable,
					vec![],
					&[],
				),
				Err(_)
			);
		});
	}

	#[test]
	fn instantiation_work_with_success_output() {
		let dummy_ch = MockLoader::insert(
			Constructor,
			|_, _| Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: vec![80, 65, 83, 83] })
		);

		ExtBuilder::default().existential_deposit(15).build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			let mut gas_meter = GasMeter::<Test>::new(GAS_LIMIT);
			let executable = MockExecutable::from_storage(
				dummy_ch, &schedule, &mut gas_meter
			).unwrap();
			set_balance(&ALICE, 1000);

			let instantiated_contract_address = assert_matches!(
				ctx.instantiate(
					100,
					&mut gas_meter,
					executable,
					vec![],
					&[],
				),
				Ok((address, ref output)) if output.data == vec![80, 65, 83, 83] => address
			);

			// Check that the newly created account has the expected code hash and
			// there are instantiation event.
			assert_eq!(Storage::<Test>::code_hash(&instantiated_contract_address).unwrap(), dummy_ch);
			assert_eq!(&events(), &[
				Event::Instantiated(ALICE, instantiated_contract_address)
			]);
		});
	}

	#[test]
	fn instantiation_fails_with_failing_output() {
		let dummy_ch = MockLoader::insert(
			Constructor,
			|_, _| Ok(ExecReturnValue { flags: ReturnFlags::REVERT, data: vec![70, 65, 73, 76] })
		);

		ExtBuilder::default().existential_deposit(15).build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			let mut gas_meter = GasMeter::<Test>::new(GAS_LIMIT);
			let executable = MockExecutable::from_storage(
				dummy_ch, &schedule, &mut gas_meter
			).unwrap();
			set_balance(&ALICE, 1000);

			let instantiated_contract_address = assert_matches!(
				ctx.instantiate(
					100,
					&mut gas_meter,
					executable,
					vec![],
					&[],
				),
				Ok((address, ref output)) if output.data == vec![70, 65, 73, 76] => address
			);

			// Check that the account has not been created.
			assert!(Storage::<Test>::code_hash(&instantiated_contract_address).is_err());
			assert!(events().is_empty());
		});
	}

	#[test]
	fn instantiation_from_contract() {
		let dummy_ch = MockLoader::insert(Call, |_, _| exec_success());
		let instantiated_contract_address = Rc::new(RefCell::new(None::<AccountIdOf<Test>>));
		let instantiator_ch = MockLoader::insert(Call, {
			let dummy_ch = dummy_ch.clone();
			let instantiated_contract_address = Rc::clone(&instantiated_contract_address);
			move |ctx, _| {
				// Instantiate a contract and save it's address in `instantiated_contract_address`.
				let (address, output, _) = ctx.ext.instantiate(
					dummy_ch,
					Contracts::<Test>::subsistence_threshold() * 3,
					ctx.gas_meter,
					vec![],
					&[48, 49, 50],
				).unwrap();

				*instantiated_contract_address.borrow_mut() = address.into();
				Ok(output)
			}
		});

		ExtBuilder::default().existential_deposit(15).build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			set_balance(&ALICE, Contracts::<Test>::subsistence_threshold() * 100);
			place_contract(&BOB, instantiator_ch);

			assert_matches!(
				ctx.call(BOB, 20, &mut GasMeter::<Test>::new(GAS_LIMIT), vec![]),
				Ok(_)
			);

			let instantiated_contract_address = instantiated_contract_address.borrow().as_ref().unwrap().clone();

			// Check that the newly created account has the expected code hash and
			// there are instantiation event.
			assert_eq!(Storage::<Test>::code_hash(&instantiated_contract_address).unwrap(), dummy_ch);
			assert_eq!(&events(), &[
				Event::Instantiated(BOB, instantiated_contract_address)
			]);
		});
	}

	#[test]
	fn instantiation_traps() {
		let dummy_ch = MockLoader::insert(Constructor,
			|_, _| Err("It's a trap!".into())
		);
		let instantiator_ch = MockLoader::insert(Call, {
			let dummy_ch = dummy_ch.clone();
			move |ctx, _| {
				// Instantiate a contract and save it's address in `instantiated_contract_address`.
				assert_matches!(
					ctx.ext.instantiate(
						dummy_ch,
						15u64,
						ctx.gas_meter,
						vec![],
						&[],
					),
					Err((ExecError {
						error: DispatchError::Other("It's a trap!"),
						origin: ErrorOrigin::Callee,
					}, 0))
				);

				exec_success()
			}
		});

		ExtBuilder::default().existential_deposit(15).build().execute_with(|| {
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			set_balance(&ALICE, 1000);
			set_balance(&BOB, 100);
			place_contract(&BOB, instantiator_ch);

			assert_matches!(
				ctx.call(BOB, 20, &mut GasMeter::<Test>::new(GAS_LIMIT), vec![]),
				Ok(_)
			);

			// The contract wasn't instantiated so we don't expect to see an instantiation
			// event here.
			assert_eq!(&events(), &[]);
		});
	}

	#[test]
	fn termination_from_instantiate_fails() {
		let terminate_ch = MockLoader::insert(Constructor, |ctx, _| {
			ctx.ext.terminate(&ALICE).unwrap();
			exec_success()
		});

		ExtBuilder::default()
			.existential_deposit(15)
			.build()
			.execute_with(|| {
				let schedule = <CurrentSchedule<Test>>::get();
				let mut ctx = MockStack::top_level(ALICE, &schedule);
				let mut gas_meter = GasMeter::<Test>::new(GAS_LIMIT);
				let executable = MockExecutable::from_storage(
					terminate_ch, &schedule, &mut gas_meter
				).unwrap();
				set_balance(&ALICE, 1000);

				assert_eq!(
					ctx.instantiate(
						100,
						&mut gas_meter,
						executable,
						vec![],
						&[],
					),
					Err(Error::<Test>::NotCallable.into())
				);

				assert_eq!(
					&events(),
					&[]
				);
			});
	}

	#[test]
	fn rent_allowance() {
		let rent_allowance_ch = MockLoader::insert(Constructor, |ctx, _| {
			let subsistence = Contracts::<Test>::subsistence_threshold();
			let allowance = subsistence * 3;
			assert_eq!(ctx.ext.rent_allowance(), <BalanceOf<Test>>::max_value());
			ctx.ext.set_rent_allowance(allowance);
			assert_eq!(ctx.ext.rent_allowance(), allowance);
			exec_success()
		});

		ExtBuilder::default().build().execute_with(|| {
			let subsistence = Contracts::<Test>::subsistence_threshold();
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			let mut gas_meter = GasMeter::<Test>::new(GAS_LIMIT);
			let executable = MockExecutable::from_storage(
				rent_allowance_ch, &schedule, &mut gas_meter
			).unwrap();
			set_balance(&ALICE, subsistence * 10);

			let result = ctx.instantiate(
				subsistence * 5,
				&mut gas_meter,
				executable,
				vec![],
				&[],
			);
			assert_matches!(result, Ok(_));
		});
	}

	#[test]
	fn rent_params_works() {
		let code_hash = MockLoader::insert(Call, |ctx, executable| {
			let address = ctx.ext.address();
			let contract = <ContractInfoOf<Test>>::get(address)
				.and_then(|c| c.get_alive())
				.unwrap();
			assert_eq!(ctx.ext.rent_params(), &RentParams::new(address, &contract, executable));
			exec_success()
		});

		ExtBuilder::default().build().execute_with(|| {
			let subsistence = Contracts::<Test>::subsistence_threshold();
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			let mut gas_meter = GasMeter::<Test>::new(GAS_LIMIT);
			set_balance(&ALICE, subsistence * 10);
			place_contract(&BOB, code_hash);
			ctx.call(
				BOB,
				0,
				&mut gas_meter,
				vec![],
			).unwrap();
		});
	}

	#[test]
	fn rent_params_snapshotted() {
		let code_hash = MockLoader::insert(Call, |ctx, executable| {
			let subsistence = Contracts::<Test>::subsistence_threshold();
			let address = ctx.ext.address();
			let contract = <ContractInfoOf<Test>>::get(address)
				.and_then(|c| c.get_alive())
				.unwrap();
			let rent_params = RentParams::new(address, &contract, executable);

			// Changing the allowance during the call: rent params stay unchanged.
			let allowance = 42;
			assert_ne!(allowance, rent_params.rent_allowance);
			ctx.ext.set_rent_allowance(allowance);
			assert_eq!(ctx.ext.rent_params(), &rent_params);

			// Creating another instance from the same code_hash increases the refcount.
			// This is also not reflected in the rent params.
			assert_eq!(MockLoader::refcount(&executable.code_hash), 1);
			ctx.ext.instantiate(
				executable.code_hash,
				subsistence * 25,
				&mut GasMeter::<Test>::new(GAS_LIMIT),
				vec![],
				&[],
			).unwrap();
			assert_eq!(MockLoader::refcount(&executable.code_hash), 2);
			assert_eq!(ctx.ext.rent_params(), &rent_params);

			exec_success()
		});

		ExtBuilder::default().build().execute_with(|| {
			let subsistence = Contracts::<Test>::subsistence_threshold();
			let schedule = <CurrentSchedule<Test>>::get();
			let mut ctx = MockStack::top_level(ALICE, &schedule);
			let mut gas_meter = GasMeter::<Test>::new(GAS_LIMIT);
			set_balance(&ALICE, subsistence * 100);
			place_contract(&BOB, code_hash);
			ctx.call(
				BOB,
				subsistence * 50,
				&mut gas_meter,
				vec![],
			).unwrap();
		});
	}
}
