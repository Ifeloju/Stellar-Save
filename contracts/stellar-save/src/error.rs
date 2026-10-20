//! Typed error enum for StellarSave contract operations.
//!
//! All user-reachable code paths return `StellarSaveError` instead of
//! panicking, so Soroban can report a structured, inspectable error code
//! rather than an opaque transaction abort.
//!
//! `StellarSaveError` is a superset of `ContractError` (from `errors.rs`) —
//! it has every variant that `ContractError` has, plus any extras that are
//! only needed internally (e.g. `TemplateNotFound`).  The two types are kept
//! separate so that the public ABI (represented by `ContractError`) can remain
//! stable while internal error handling is extended freely.

use soroban_sdk::{contracterror, contracttype};

/// Typed errors for all contract operations.
///
/// Each variant has a stable numeric code that external clients can rely on
/// across contract versions.  Error codes mirror `ContractError` for the
/// shared variants so that tooling only needs to know one set of codes.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum StellarSaveError {
    // ── Group (1000–1999) ──────────────────────────────────────────────────
    /// The specified group ID does not exist.
    GroupNotFound = 1001,
    /// The group has reached its maximum member capacity.
    GroupFull = 1002,
    /// The group is not in a valid state for the requested operation.
    InvalidState = 1003,
    /// Invalid metadata provided (name, description, or image_url).
    InvalidMetadata = 1004,
    /// The requested max_members exceeds the protocol-level MAX_MEMBERS cap.
    MaxMembersExceeded = 1005,
    /// A dispute is currently active for this group; payouts are blocked.
    DisputeActive = 1006,
    /// The group cannot be archived because it is not in a terminal state.
    GroupNotArchivable = 1007,
    /// The two groups are not compatible for merging.
    MergeIncompatible = 1008,

    // ── Member (2000–2999) ─────────────────────────────────────────────────
    /// The address is already a member of this group.
    AlreadyMember = 2001,
    /// The address is not a member of this group.
    NotMember = 2002,
    /// The caller is not authorized to perform this operation.
    Unauthorized = 2003,
    /// The address has not been invited to join this invitation-only group.
    NotInvited = 2004,
    /// The member has already voted on the current dispute.
    AlreadyVoted = 2005,

    // ── Contribution (3000–3999) ───────────────────────────────────────────
    /// The contribution amount is invalid (zero, negative, or incorrect).
    InvalidAmount = 3001,
    /// The member has already contributed for the current cycle.
    AlreadyContributed = 3002,
    /// The current cycle is not complete (missing contributions).
    CycleNotComplete = 3003,
    /// The contribution record was not found.
    ContributionNotFound = 3004,
    /// The cycle deadline has not yet been reached.
    DeadlineNotReached = 3005,
    /// The contribution amount is below the configured minimum.
    ContributionTooLow = 3006,
    /// The contribution amount exceeds the configured maximum.
    ContributionTooHigh = 3007,
    /// The member's token balance is insufficient for auto-contribution.
    InsufficientBalance = 3008,
    /// The cycle deadline has passed; contributions are no longer accepted.
    CycleDeadlineExpired = 3009,
    /// The submitted contribution zk proof failed cryptographic verification.
    InvalidProof = 3010,

    // ── Payout (4000–4999) ─────────────────────────────────────────────────
    /// The payout operation failed due to insufficient funds or transfer error.
    PayoutFailed = 4001,
    /// The payout has already been processed for this cycle.
    PayoutAlreadyProcessed = 4002,
    /// The recipient is not eligible for payout in this cycle.
    InvalidRecipient = 4003,

    // ── Token (5000–5999) ──────────────────────────────────────────────────
    /// The token address failed SEP-41 validation or is not on the allowlist.
    InvalidToken = 5001,
    /// The SEP-41 transfer call failed during contribution or payout.
    TokenTransferFailed = 5002,

    // ── Reward (6000–6999) ─────────────────────────────────────────────────
    /// The member has already claimed their completion reward.
    RewardAlreadyClaimed = 6001,
    /// The member is not eligible to claim a completion reward.
    RewardNotEligible = 6002,
    /// The contribution has already been refunded.
    AlreadyRefunded = 6003,
    /// Refund is not eligible.
    RefundNotEligible = 6004,

    // ── Deadline (7000–7999) ───────────────────────────────────────────────
    /// The requested deadline extension exceeds the maximum allowed (7 days).
    DeadlineExtensionExceedsMax = 7001,
    /// The member has already voted to dissolve this group.
    AlreadyVotedDissolve = 7002,
    /// The group has already been dissolved.
    GroupAlreadyDissolved = 7003,

    // ── Templates (8000–8999) ──────────────────────────────────────────────
    /// The specified template ID does not exist.
    TemplateNotFound = 8001,

    // ── System (9000–9999) ─────────────────────────────────────────────────
    /// An internal contract error occurred.
    InternalError = 9001,
    /// The contract data is corrupted or invalid.
    DataCorruption = 9002,
    /// Arithmetic overflow (e.g. checked_add / checked_mul returned None).
    Overflow = 9003,
    /// Reentrancy detected during function execution.
    ReentrancyDetected = 9004,
}

/// Convenience alias: every fallible contract function returns this type.
pub type ContractResult<T> = Result<T, StellarSaveError>;

/// Error categories for grouping related error types.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ErrorCategory {
    /// Errors related to group operations and state.
    Group,
    /// Errors related to member management and permissions.
    Member,
    /// Errors related to contribution processing.
    Contribution,
    /// Errors related to payout operations.
    Payout,
    /// Errors related to token validation and transfer operations.
    Token,
    /// Errors related to completion reward operations.
    Reward,
    /// Errors related to deadline extension operations.
    Deadline,
    /// Errors related to group templates.
    Template,
    /// System-level errors and internal failures.
    System,
    /// Unknown or uncategorized errors.
    Unknown,
}

impl StellarSaveError {
    /// Returns the numeric error code.
    pub fn code(self) -> u32 {
        self as u32
    }

    /// Returns the error category based on the error code range.
    pub fn category(self) -> ErrorCategory {
        match self.code() {
            1000..=1999 => ErrorCategory::Group,
            2000..=2999 => ErrorCategory::Member,
            3000..=3999 => ErrorCategory::Contribution,
            4000..=4999 => ErrorCategory::Payout,
            5000..=5999 => ErrorCategory::Token,
            6000..=6999 => ErrorCategory::Reward,
            7000..=7999 => ErrorCategory::Deadline,
            8000..=8999 => ErrorCategory::Template,
            9000..=9999 => ErrorCategory::System,
            _ => ErrorCategory::Unknown,
        }
    }
}
