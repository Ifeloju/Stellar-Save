//! Contribution entry-point implementations for StellarSaveContract.
//!
//! Covers: contribute, contribute_batch, auto-contribution management,
//! contribution proof verification, and dynamic contribution amounts.

use crate::error::StellarSaveError;
use crate::events::EventEmitter;
use crate::group::{Group, GroupStatus};
use crate::milestones;
use crate::storage::StorageKeyBuilder;
use crate::types::{ContractConfig, MemberProfile};
use crate::{governance, zk_proof};
use soroban_sdk::{Address, Bytes, Env, Vec};

// ── Internal helpers (re-used from contract.rs) ───────────────────────────────

/// Records a contribution in storage and updates member statistics.
///
/// Returns the new `cycle_total` so the caller can use it directly in the
/// event emission without an extra SLOAD.
pub fn record_contribution(
    env: &Env,
    group_id: u64,
    cycle_number: u32,
    member_address: Address,
    amount: i128,
    timestamp: u64,
) -> Result<i128, StellarSaveError> {
    // 1. Check if member has already contributed in this cycle (1 SLOAD)
    let contrib_key = StorageKeyBuilder::contribution_individual(
        group_id,
        cycle_number,
        member_address.clone(),
    );

    if env.storage().persistent().has(&contrib_key) {
        return Err(StellarSaveError::AlreadyContributed);
    }

    record_contribution_unchecked(env, group_id, cycle_number, member_address, amount, timestamp)
}

/// Records a contribution without repeating the `has(&contrib_key)` SLOAD.
/// Callers that have already validated contribution status can bypass the redundant storage read.
pub fn record_contribution_unchecked(
    env: &Env,
    group_id: u64,
    cycle_number: u32,
    member_address: Address,
    amount: i128,
    timestamp: u64,
) -> Result<i128, StellarSaveError> {
    use crate::contribution::ContributionRecord;

    let contrib_key = StorageKeyBuilder::contribution_individual(
        group_id,
        cycle_number,
        member_address.clone(),
    );

    // 2. Create and store contribution record (1 SSTORE)
    let contribution = ContributionRecord::new(
        member_address.clone(),
        group_id,
        cycle_number,
        amount,
        timestamp,
    );
    env.storage().persistent().set(&contrib_key, &contribution);

    // 3. Update cycle total amount (1 SLOAD + 1 SSTORE)
    let total_key = StorageKeyBuilder::contribution_cycle_total(group_id, cycle_number);
    let current_total: i128 = env.storage().persistent().get(&total_key).unwrap_or(0);
    let new_total = current_total
        .checked_add(amount)
        .ok_or(StellarSaveError::Overflow)?;
    env.storage().persistent().set(&total_key, &new_total);

    // 4. Update cycle contributor count (1 SLOAD + 1 SSTORE)
    let count_key = StorageKeyBuilder::contribution_cycle_count(group_id, cycle_number);
    let current_count: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
    let new_count = current_count
        .checked_add(1)
        .ok_or(StellarSaveError::Overflow)?;
    env.storage().persistent().set(&count_key, &new_count);

    // 5. Update incremental group balance counter (1 SLOAD + 1 SSTORE)
    let balance_key = StorageKeyBuilder::group_balance(group_id);
    let current_balance: i128 = env.storage().persistent().get(&balance_key).unwrap_or(0);
    let new_balance = current_balance
        .checked_add(amount)
        .ok_or(StellarSaveError::Overflow)?;
    env.storage().persistent().set(&balance_key, &new_balance);

    // 6. Update contribution streak and emit milestone events if thresholds crossed
    milestones::update_streak(env, group_id, member_address, cycle_number);

    Ok(new_total)
}

// ── Public entry-points ───────────────────────────────────────────────────────

/// Records a member's contribution for the current cycle of a group.
///
/// # Arguments
/// * `env` - Soroban environment
/// * `group_id` - ID of the group to contribute to
/// * `member` - Address of the contributing member (must be caller)
/// * `amount` - Contribution amount in token base units (must match group's contribution_amount)
///
/// # Returns
/// * `Ok(())` - Contribution successfully recorded
pub fn contribute(
    env: Env,
    group_id: u64,
    member: Address,
    amount: i128,
) -> Result<(), StellarSaveError> {
    // Require authorization from the member
    member.require_auth();

    // ── Step 1: Load group once — reuse for all subsequent checks ──────────
    let group_key = StorageKeyBuilder::group_data(group_id);
    let group: Group = env
        .storage()
        .persistent()
        .get(&group_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    // ── Step 2: Validate group is Active ───────────────────────────────────
    if group.status != GroupStatus::Active {
        return Err(StellarSaveError::InvalidState);
    }

    // ── Step 2b: Deadline + grace period check ─────────────────────────────
    let now = env.ledger().timestamp();
    let is_grace_period = if group.started {
        let cycle_deadline = group.started_at.saturating_add(
            group
                .cycle_duration
                .saturating_mul(group.current_cycle as u64 + 1),
        );
        if now > cycle_deadline.saturating_add(group.grace_period_seconds) {
            return Err(StellarSaveError::CycleDeadlineExpired);
        }
        (now > cycle_deadline, now.saturating_sub(cycle_deadline))
    } else {
        (false, 0u64)
    };

    // ── Step 3: Validate member is part of the group (1 SLOAD) ────────────
    let member_key = StorageKeyBuilder::member_profile(group_id, member.clone());
    if !env.storage().persistent().has(&member_key) {
        return Err(StellarSaveError::NotMember);
    }

    // ── Step 4: Validate amount against in-memory group (0 extra SLOADs) ──
    if amount != group.contribution_amount {
        return Err(StellarSaveError::InvalidAmount);
    }

    // ── Step 5: Reentrancy guard using temporary storage ──────────────────
    let reentrancy_key = StorageKeyBuilder::reentrancy_guard();
    let guard_value: u64 = env.storage().temporary().get(&reentrancy_key).unwrap_or(0);
    if guard_value != 0 {
        return Err(StellarSaveError::InternalError);
    }
    env.storage().temporary().set(&reentrancy_key, &1u64);

    // ── Step 6: Load TokenConfig (1 SLOAD) ────────────────────────────────
    let token_config_key = StorageKeyBuilder::group_token_config(group_id);
    let token_config: crate::group::TokenConfig = env
        .storage()
        .persistent()
        .get(&token_config_key)
        .ok_or_else(|| {
            env.storage().temporary().set(&reentrancy_key, &0u64);
            StellarSaveError::GroupNotFound
        })?;

    // ── Step 7: Record contribution (effects) ─────────────────────────────
    let timestamp = env.ledger().timestamp();
    let current_cycle = group.current_cycle;

    let cycle_total = record_contribution(
        &env,
        group_id,
        current_cycle,
        member.clone(),
        amount,
        timestamp,
    )?;

    // ── Step 8: SEP-41 transfer_from (interaction) ────────────────────────
    let token_client = soroban_sdk::token::TokenClient::new(&env, &token_config.token_address);
    let contract_address = env.current_contract_address();

    token_client.transfer_from(&contract_address, &member, &contract_address, &amount);

    // Release the reentrancy guard only after the external call has returned
    env.storage().temporary().set(&reentrancy_key, &0u64);

    // ── Step 9: Emit event ────────────────────────────────────────────────
    EventEmitter::emit_contribution_made(
        &env,
        group_id,
        member.clone(),
        amount,
        current_cycle,
        cycle_total,
        timestamp,
    );

    // Emit grace period event if contribution landed after the hard deadline
    if is_grace_period.0 {
        EventEmitter::emit_grace_period_contribution(
            &env,
            group_id,
            member,
            amount,
            current_cycle,
            is_grace_period.1,
            timestamp,
        );
    }

    Ok(())
}

/// Pre-pays contributions for multiple upcoming cycles in a single transaction.
///
/// # Arguments
/// * `group_id` - The group to contribute to
/// * `member`   - The member making the contributions (must authorize)
/// * `cycles`   - List of cycle numbers to pre-pay (must be unique, unpaid, ≥ current cycle)
pub fn contribute_batch(
    env: Env,
    group_id: u64,
    member: Address,
    cycles: Vec<u32>,
) -> Result<(), StellarSaveError> {
    member.require_auth();

    // Reject empty input early
    if cycles.is_empty() {
        return Err(StellarSaveError::InvalidAmount);
    }

    // Load group once
    let group_key = StorageKeyBuilder::group_data(group_id);
    let group: Group = env
        .storage()
        .persistent()
        .get(&group_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    if group.status != GroupStatus::Active {
        return Err(StellarSaveError::InvalidState);
    }

    // Verify member
    let member_key = StorageKeyBuilder::member_profile(group_id, member.clone());
    if !env.storage().persistent().has(&member_key) {
        return Err(StellarSaveError::NotMember);
    }

    // Validate all cycles before touching token or storage
    for i in 0..cycles.len() {
        let cycle = cycles.get(i).unwrap();
        if cycle < group.current_cycle {
            return Err(StellarSaveError::CycleDeadlineExpired);
        }
        let contrib_key =
            StorageKeyBuilder::contribution_individual(group_id, cycle, member.clone());
        if env.storage().persistent().has(&contrib_key) {
            return Err(StellarSaveError::AlreadyContributed);
        }
        // Duplicate detection
        for j in 0..i {
            if cycles.get(j).unwrap() == cycle {
                return Err(StellarSaveError::AlreadyContributed);
            }
        }
    }

    // Load token config once
    let token_config_key = StorageKeyBuilder::group_token_config(group_id);
    let token_config: crate::group::TokenConfig = env
        .storage()
        .persistent()
        .get(&token_config_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    let token_client = soroban_sdk::token::TokenClient::new(&env, &token_config.token_address);
    let contract_address = env.current_contract_address();
    let amount = group.contribution_amount;
    let timestamp = env.ledger().timestamp();

    let total = amount
        .checked_mul(cycles.len() as i128)
        .ok_or(StellarSaveError::Overflow)?;

    // ── Effects: record each cycle before touching token ─────────────────
    let mut cycle_totals: Vec<i128> = soroban_sdk::Vec::new(&env);
    for i in 0..cycles.len() {
        let cycle = cycles.get(i).unwrap();
        cycle_totals.push_back(record_contribution_unchecked(
            &env,
            group_id,
            cycle,
            member.clone(),
            amount,
            timestamp,
        )?);
    }

    // ── Interaction ───────────────────────────────────────────────────────
    token_client.transfer_from(&contract_address, &member, &contract_address, &total);

    // Emit one ContributionEvent per cycle now that the funds have landed
    for i in 0..cycles.len() {
        EventEmitter::emit_contribution_made(
            &env,
            group_id,
            member.clone(),
            amount,
            cycles.get(i).unwrap(),
            cycle_totals.get(i).unwrap(),
            timestamp,
        );
    }

    Ok(())
}

/// Enables automatic contributions for a member in a group.
pub fn enable_auto_contribute(
    env: Env,
    group_id: u64,
    member: Address,
) -> Result<(), StellarSaveError> {
    member.require_auth();

    let group_key = StorageKeyBuilder::group_data(group_id);
    let group: Group = env
        .storage()
        .persistent()
        .get(&group_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    if group.status != GroupStatus::Active {
        return Err(StellarSaveError::InvalidState);
    }

    let member_key = StorageKeyBuilder::member_profile(group_id, member.clone());
    let mut profile: MemberProfile = env
        .storage()
        .persistent()
        .get(&member_key)
        .ok_or(StellarSaveError::NotMember)?;

    // Idempotent: no-op if already enabled
    if profile.auto_contribute_enabled {
        return Ok(());
    }

    profile.auto_contribute_enabled = true;
    env.storage().persistent().set(&member_key, &profile);

    let flag_key = StorageKeyBuilder::member_auto_contribute(group_id, member.clone());
    env.storage().persistent().set(&flag_key, &true);

    Ok(())
}

/// Disables automatic contributions for a member in a group.
pub fn disable_auto_contribute(
    env: Env,
    group_id: u64,
    member: Address,
) -> Result<(), StellarSaveError> {
    member.require_auth();

    let group_key = StorageKeyBuilder::group_data(group_id);
    if !env.storage().persistent().has(&group_key) {
        return Err(StellarSaveError::GroupNotFound);
    }

    let member_key = StorageKeyBuilder::member_profile(group_id, member.clone());
    let mut profile: MemberProfile = env
        .storage()
        .persistent()
        .get(&member_key)
        .ok_or(StellarSaveError::NotMember)?;

    // Idempotent: no-op if already disabled
    if !profile.auto_contribute_enabled {
        return Ok(());
    }

    profile.auto_contribute_enabled = false;
    env.storage().persistent().set(&member_key, &profile);

    let flag_key = StorageKeyBuilder::member_auto_contribute(group_id, member.clone());
    env.storage().persistent().remove(&flag_key);

    Ok(())
}

/// Returns whether auto-contribution is enabled for a member in a group.
pub fn is_auto_contribute_enabled(
    env: Env,
    group_id: u64,
    member: Address,
) -> Result<bool, StellarSaveError> {
    let group_key = StorageKeyBuilder::group_data(group_id);
    if !env.storage().persistent().has(&group_key) {
        return Err(StellarSaveError::GroupNotFound);
    }

    let member_key = StorageKeyBuilder::member_profile(group_id, member.clone());
    if !env.storage().persistent().has(&member_key) {
        return Err(StellarSaveError::NotMember);
    }

    let flag_key = StorageKeyBuilder::member_auto_contribute(group_id, member);
    Ok(env
        .storage()
        .persistent()
        .get::<_, bool>(&flag_key)
        .unwrap_or(false))
}

/// Executes automatic contributions for all opted-in members of a group.
///
/// This is a permissionless function — anyone can call it to trigger
/// auto-contributions at the start of a new cycle.
pub fn execute_auto_contributions(env: Env, group_id: u64) -> Result<u32, StellarSaveError> {
    use soroban_sdk::Map;

    // ── 1. Load group ─────────────────────────────────────────────────────
    let group_key = StorageKeyBuilder::group_data(group_id);
    let group: Group = env
        .storage()
        .persistent()
        .get(&group_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    if group.status != GroupStatus::Active {
        return Err(StellarSaveError::InvalidState);
    }

    let current_cycle = group.current_cycle;
    let contribution_amount = group.contribution_amount;

    // ── 2. Load token config once ─────────────────────────────────────────
    let token_config_key = StorageKeyBuilder::group_token_config(group_id);
    let token_config: crate::group::TokenConfig = env
        .storage()
        .persistent()
        .get(&token_config_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    let token_client = soroban_sdk::token::TokenClient::new(&env, &token_config.token_address);
    let contract_address = env.current_contract_address();

    // ── 3. Load member list ───────────────────────────────────────────────
    let members_key = StorageKeyBuilder::group_members(group_id);
    let members: Map<u32, Address> = env
        .storage()
        .persistent()
        .get(&members_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    let timestamp = env.ledger().timestamp();
    let mut executed_count: u32 = 0;

    // ── 4. Iterate members ────────────────────────────────────────────────
    for (_, member) in members.iter() {
        // 4a. Check if auto-contribute is enabled for this member
        let flag_key = StorageKeyBuilder::member_auto_contribute(group_id, member.clone());
        let auto_enabled: bool = env
            .storage()
            .persistent()
            .get::<_, bool>(&flag_key)
            .unwrap_or(false);

        if !auto_enabled {
            continue;
        }

        // 4b. Skip if member already contributed this cycle
        let contrib_key =
            StorageKeyBuilder::contribution_individual(group_id, current_cycle, member.clone());
        if env.storage().persistent().has(&contrib_key) {
            continue;
        }

        // 4c. Check member balance — soft failure on insufficient funds
        let balance = token_client.balance(&member);
        if balance < contribution_amount {
            EventEmitter::emit_auto_contribution_failed(
                &env,
                group_id,
                member.clone(),
                current_cycle,
                timestamp,
            );
            continue;
        }

        // 4d. Check allowance — soft failure if allowance is insufficient
        let allowance = token_client.allowance(&member, &contract_address);
        if allowance < contribution_amount {
            EventEmitter::emit_auto_contribution_failed(
                &env,
                group_id,
                member.clone(),
                current_cycle,
                timestamp,
            );
            continue;
        }

        // 4e. Record contribution in storage (effects before interaction)
        let cycle_total = record_contribution_unchecked(
            &env,
            group_id,
            current_cycle,
            member.clone(),
            contribution_amount,
            timestamp,
        )?;

        // 4f. Execute the transfer (interaction)
        token_client.transfer_from(
            &contract_address,
            &member,
            &contract_address,
            &contribution_amount,
        );

        // 4g. Emit AutoContributionExecuted event
        EventEmitter::emit_auto_contribution_executed(
            &env,
            group_id,
            member.clone(),
            contribution_amount,
            current_cycle,
            timestamp,
        );

        // 4h. Also emit the standard ContributionMade event for consistency
        EventEmitter::emit_contribution_made(
            &env,
            group_id,
            member,
            contribution_amount,
            current_cycle,
            cycle_total,
            timestamp,
        );

        executed_count += 1;
    }

    Ok(executed_count)
}

/// Enables or disables contribution proof requirement for a group.
/// Only the group creator can call this while the group is Pending.
pub fn set_contribution_proof_required(
    env: Env,
    group_id: u64,
    required: bool,
) -> Result<(), StellarSaveError> {
    let group_key = StorageKeyBuilder::group_data(group_id);
    let mut group = env
        .storage()
        .persistent()
        .get::<_, Group>(&group_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    group.creator.require_auth();

    group.require_contribution_proof = required;
    env.storage().persistent().set(&group_key, &group);
    Ok(())
}

/// Verifies a contribution proof for a member in a cycle.
pub fn verify_contribution_proof(
    env: Env,
    group_id: u64,
    member: Address,
    cycle: u32,
    proof: Bytes,
) -> Result<(), StellarSaveError> {
    let group_key = StorageKeyBuilder::group_data(group_id);
    let group = env
        .storage()
        .persistent()
        .get::<_, Group>(&group_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    if !group.require_contribution_proof {
        return Err(StellarSaveError::InvalidState);
    }

    let member_key = StorageKeyBuilder::member_profile(group_id, member.clone());
    if !env.storage().persistent().has(&member_key) {
        return Err(StellarSaveError::NotMember);
    }

    if !zk_proof::verify_contribution_proof(&env, &proof, group_id, cycle) {
        return Err(StellarSaveError::InvalidProof);
    }

    let proof_key =
        StorageKeyBuilder::contribution_proof_verified(group_id, cycle, member.clone());
    env.storage().persistent().set(&proof_key, &true);

    let timestamp = env.ledger().timestamp();
    EventEmitter::emit_contribution_verified(&env, group_id, member, cycle, timestamp);

    Ok(())
}

/// Records a contribution for a group that requires proof verification.
///
/// The member must have called `verify_contribution_proof` for this cycle
/// before calling this function.
pub fn contribute_with_proof(
    env: Env,
    group_id: u64,
    member: Address,
    amount: i128,
) -> Result<(), StellarSaveError> {
    member.require_auth();

    let group_key = StorageKeyBuilder::group_data(group_id);
    let group = env
        .storage()
        .persistent()
        .get::<_, Group>(&group_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    if group.status != GroupStatus::Active {
        return Err(StellarSaveError::InvalidState);
    }

    if amount != group.contribution_amount {
        return Err(StellarSaveError::InvalidAmount);
    }

    let member_key = StorageKeyBuilder::member_profile(group_id, member.clone());
    if !env.storage().persistent().has(&member_key) {
        return Err(StellarSaveError::NotMember);
    }

    // If proof is required, check it was verified
    if group.require_contribution_proof {
        let proof_key = StorageKeyBuilder::contribution_proof_verified(
            group_id,
            group.current_cycle,
            member.clone(),
        );
        if !env
            .storage()
            .persistent()
            .get::<_, bool>(&proof_key)
            .unwrap_or(false)
        {
            return Err(StellarSaveError::Unauthorized);
        }
    }

    let timestamp = env.ledger().timestamp();
    record_contribution(
        &env,
        group_id,
        group.current_cycle,
        member.clone(),
        amount,
        timestamp,
    )?;

    let cycle_total_key =
        StorageKeyBuilder::contribution_cycle_total(group_id, group.current_cycle);
    let cycle_total: i128 = env
        .storage()
        .persistent()
        .get(&cycle_total_key)
        .unwrap_or(0);

    EventEmitter::emit_contribution_made(
        &env,
        group_id,
        member,
        amount,
        group.current_cycle,
        cycle_total,
        timestamp,
    );

    Ok(())
}

/// Enables or disables dynamic contribution amounts for a group.
/// Only the group creator can call this while the group is Pending.
pub fn set_dynamic_contributions(
    env: Env,
    group_id: u64,
    allowed: bool,
) -> Result<(), StellarSaveError> {
    let group_key = StorageKeyBuilder::group_data(group_id);
    let mut group = env
        .storage()
        .persistent()
        .get::<_, Group>(&group_key)
        .ok_or(StellarSaveError::GroupNotFound)?;

    group.creator.require_auth();

    if group.status != GroupStatus::Pending {
        return Err(StellarSaveError::InvalidState);
    }

    group.allow_dynamic_contributions = allowed;
    env.storage().persistent().set(&group_key, &group);
    Ok(())
}

/// Proposes a new contribution amount for the next cycle.
/// Only the group creator can propose; the group must allow dynamic contributions.
pub fn propose_contribution_change(
    env: Env,
    group_id: u64,
    new_amount: i128,
) -> Result<(), StellarSaveError> {
    governance::propose_contribution_change(env, group_id, new_amount)
}

/// Casts a member's vote to approve the pending contribution amount change.
/// When a majority (> 50%) of members approve, the change is applied immediately.
pub fn vote_contribution_change(
    env: Env,
    group_id: u64,
    member: Address,
) -> Result<(), StellarSaveError> {
    governance::vote_contribution_change(env, group_id, member)
}
