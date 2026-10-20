//! Admin entry-point implementations for StellarSaveContract.
//!
//! Covers: contract config, migration, version management, contribution limits,
//! and the penalty system configuration.

use crate::error::StellarSaveError;
use crate::migration::{initialize_storage_version, migrate};
use crate::storage::StorageKeyBuilder;
use crate::types::ContractConfig;
use crate::{migration, penalty};
use soroban_sdk::{Address, BytesN, Env};

// ── Re-export auth for internal use ──────────────────────────────────────────
use crate::auth;

/// Initializes or updates the global contract configuration.
///
/// This function also handles storage migration when needed and initializes
/// the storage version on first deployment.
/// Only the current admin can perform this update.
pub fn update_config(env: Env, new_config: ContractConfig) -> Result<(), StellarSaveError> {
    // 1. Validation Logic
    if !new_config.validate() {
        return Err(StellarSaveError::InvalidState);
    }

    let key = StorageKeyBuilder::contract_config();

    // 2. Admin-only Authorization
    if let Some(current_config) = env.storage().persistent().get::<_, ContractConfig>(&key) {
        current_config.admin.require_auth();
    } else {
        // First time initialization: caller becomes admin
        new_config.admin.require_auth();

        // Initialize storage version on first deployment
        initialize_storage_version(&env);
        // Initialize contract binary version on first deployment (Issue #72)
        migration::initialize_contract_version(&env);
    }

    // 3. Perform migration if needed
    migrate(&env)?;

    // 4. Save Configuration
    env.storage().persistent().set(&key, &new_config);
    Ok(())
}

/// Performs storage migration to the latest schema version.
///
/// This function can be called by the admin to manually trigger migration
/// without updating the contract configuration.
///
/// # Returns
/// * `Ok(())` - If migration completed successfully or no migration needed
/// * `Err(StellarSaveError)` - If migration failed or caller is not admin
pub fn migrate_storage(env: Env, caller: Address) -> Result<(), StellarSaveError> {
    // Require admin authorization via shared helper
    auth::require_admin(&env, &caller)?;

    // Perform migration
    migrate(&env)?;

    Ok(())
}

/// Gets the current storage schema version.
///
/// Returns the version number of the storage schema currently in use.
/// This can be used to check if migration is needed.
pub fn get_storage_version(env: Env) -> u32 {
    migration::get_storage_version(&env)
}

/// Returns the on-chain contract binary version.
///
/// This is the monotonically increasing version number that the upgrade
/// guard tracks to prevent downgrade or double-upgrade (Issue #72).
pub fn get_contract_version(env: Env) -> u32 {
    migration::get_contract_version(&env)
}

/// Upgrades the contract Wasm to a new binary, enforcing the version guard.
///
/// Only the contract admin may call this.  `new_version` must be strictly
/// greater than the current on-chain contract version, which prevents
/// accidental downgrade or double-upgrade (Issue #72).
///
/// # Arguments
/// * `caller`      – Must be the contract admin
/// * `new_wasm`    – 32-byte Wasm hash of the replacement binary
/// * `new_version` – New version number (must be > current on-chain version)
///
/// # Returns
/// * `Ok(())` – Upgrade applied, version persisted, event emitted
/// * `Err(StellarSaveError::Unauthorized)` – Caller is not the admin
/// * `Err(StellarSaveError::InvalidState)` – Version guard rejected the upgrade
pub fn upgrade_contract(
    env: Env,
    caller: Address,
    new_wasm: BytesN<32>,
    new_version: u32,
) -> Result<(), StellarSaveError> {
    auth::require_admin(&env, &caller)?;

    migration::execute_upgrade(&env, caller, new_wasm, new_version)
}

/// Updates the global contribution amount limits.
///
/// Only the contract admin can call this function.
///
/// # Arguments
/// * `env` - Soroban environment
/// * `admin` - Admin address (must match stored config admin)
/// * `min_contribution` - New minimum contribution amount (must be > 0)
/// * `max_contribution` - New maximum contribution amount (must be >= min_contribution)
///
/// # Returns
/// * `Ok(())` - Limits updated successfully
/// * `Err(StellarSaveError::Unauthorized)` - Caller is not the admin
/// * `Err(StellarSaveError::ContributionTooLow)` - min_contribution <= 0
/// * `Err(StellarSaveError::ContributionTooHigh)` - max_contribution < min_contribution
pub fn update_contribution_limits(
    env: Env,
    admin: Address,
    min_contribution: i128,
    max_contribution: i128,
) -> Result<(), StellarSaveError> {
    auth::require_admin(&env, &admin)?;

    if min_contribution <= 0 {
        return Err(StellarSaveError::ContributionTooLow);
    }
    if max_contribution < min_contribution {
        return Err(StellarSaveError::ContributionTooHigh);
    }

    let key = StorageKeyBuilder::contract_config();
    let mut config = env
        .storage()
        .persistent()
        .get::<_, ContractConfig>(&key)
        .ok_or(StellarSaveError::Unauthorized)?;

    config.min_contribution = min_contribution;
    config.max_contribution = max_contribution;
    env.storage().persistent().set(&key, &config);

    Ok(())
}

/// Applies a penalty to a member who missed a contribution deadline.
///
/// Called by the group creator or automatically during cycle advancement.
/// Deducts a percentage of the contribution amount from the group balance
/// and records the event in the member's penalty history.
///
/// # Arguments
/// * `env` - Soroban environment
/// * `group_id` - ID of the group
/// * `member` - Address of the member who missed the contribution
/// * `cycle_id` - The cycle that was missed
///
/// # Returns
/// * `Ok(i128)` - Penalty amount deducted in stroops
/// * `Err(StellarSaveError)` - If group/member not found or overflow
pub fn apply_penalty(
    env: Env,
    group_id: u64,
    member: Address,
    cycle_id: u32,
) -> Result<i128, StellarSaveError> {
    penalty::apply_penalty(&env, group_id, member, cycle_id)
}

/// Allows a member to recover from a penalty by paying the missed
/// contribution plus a recovery fee (default 10% of contribution amount).
///
/// # Arguments
/// * `env` - Soroban environment
/// * `group_id` - ID of the group
/// * `member` - Address of the recovering member
/// * `cycle_id` - The cycle being recovered
/// * `amount_paid` - Total amount paid (must be >= contribution + recovery fee)
///
/// # Returns
/// * `Ok(())` - Recovery successful
/// * `Err(StellarSaveError)` - If validation fails
pub fn recover_penalty(
    env: Env,
    group_id: u64,
    member: Address,
    cycle_id: u32,
    amount_paid: i128,
) -> Result<(), StellarSaveError> {
    member.require_auth();
    penalty::recover_penalty(&env, group_id, member, cycle_id, amount_paid)
}

/// Returns the full penalty history for a member in a group.
///
/// # Arguments
/// * `env` - Soroban environment
/// * `group_id` - ID of the group
/// * `member` - Address of the member
///
/// # Returns
/// * `Vec<PenaltyRecord>` - List of penalty records (empty if none)
pub fn get_penalty_history(
    env: Env,
    group_id: u64,
    member: Address,
) -> penalty::PenaltyRecordVec {
    penalty::get_penalty_history(&env, group_id, member)
}

/// Returns the current penalty state (missed cycles, total penalty) for a member.
///
/// # Arguments
/// * `env` - Soroban environment
/// * `group_id` - ID of the group
/// * `member` - Address of the member
///
/// # Returns
/// * `MemberPenaltyState` - Current penalty state
pub fn get_penalty_state(
    env: Env,
    group_id: u64,
    member: Address,
) -> penalty::MemberPenaltyState {
    penalty::get_penalty_state(&env, group_id, member)
}

/// Sets a custom penalty configuration for a group.
/// Only the group creator can call this while the group is Pending.
///
/// # Arguments
/// * `env` - Soroban environment
/// * `group_id` - ID of the group
/// * `caller` - Must be the group creator
/// * `config` - New penalty configuration
///
/// # Returns
/// * `Ok(())` - Config updated
/// * `Err(StellarSaveError)` - If unauthorized or group not found
pub fn set_penalty_config(
    env: Env,
    group_id: u64,
    caller: Address,
    config: penalty::PenaltyConfig,
) -> Result<(), StellarSaveError> {
    use crate::group::Group;
    let group_key = StorageKeyBuilder::group_data(group_id);
    let group = env
        .storage()
        .persistent()
        .get::<_, Group>(&group_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    auth::require_creator(&caller, &group)?;

    penalty::set_penalty_config(&env, group_id, config);
    Ok(())
}
