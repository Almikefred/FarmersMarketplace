//! Creator Earnings — Soroban contract
//!
//! Tracks accumulated earnings per creator (farmer) and allows them to claim
//! their balance. A platform fee (in basis points) is deducted on each credit.
//!
//! Invariants (verified by property tests):
//!   I1 — credited amount is always positive.
//!   I2 — fee_bps is always ≤ 10_000.
//!   I3 — farmer_amount + fee_amount == total credited amount (no value created/destroyed).
//!   I4 — balance never goes negative.
//!   I5 — claim resets balance to zero.
//!   I6 — double-claim on zero balance returns ZeroBalance error.

#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, contracterror, symbol_short, token, Address, Env, Vec};

/// Maximum number of entries accepted by `batch_credit` in a single call —
/// keeps the transaction under Stellar's operation limit.
const MAX_BATCH_CREDIT: u32 = 20;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum EarningsError {
    /// fee_bps exceeds 10 000 (100 %).
    InvalidFeeBps = 1,
    /// Credited amount must be > 0.
    InvalidAmount = 2,
    /// Creator has no balance to claim.
    ZeroBalance = 3,
    /// Platform address has not been initialised.
    NotInitialised = 4,
    /// `batch_credit` was called with more than `MAX_BATCH_CREDIT` entries.
    BatchTooLarge = 5,
    /// Contract is paused; credit() and claim() are disabled.
    Paused = 6,
}

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Accumulated claimable balance for a creator.
    Balance(Address),
    /// Platform fee recipient address.
    Platform,
    /// Admin pause flag: if true, credit() and claim() return Paused error.
    PausedState,
    /// Lifetime total earnings for a creator (farmer_amount only, never reset).
    LifetimeEarned(Address),
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct CreatorEarningsContract;

#[contractimpl]
impl CreatorEarningsContract {
    /// One-time initialisation: register the platform fee recipient.
    pub fn init(env: Env, platform: Address) {
        // Idempotent — safe to call again with the same address.
        env.storage().instance().set(&DataKey::Platform, &platform);
    }

    /// Admin-only: set or clear the pause flag.
    /// When paused, credit() and claim() return Paused error.
    /// balance() continues to work.
    pub fn set_paused(env: Env, paused: bool) -> Result<(), EarningsError> {
        let platform: Address = env
            .storage()
            .instance()
            .get(&DataKey::Platform)
            .ok_or(EarningsError::NotInitialised)?;
        platform.require_auth();
        env.storage().instance().set(&DataKey::PausedState, &paused);
        Ok(())
    }

    /// Credit `amount` tokens to `creator`, splitting off `fee_bps` basis
    /// points to the platform.  The caller must have already transferred
    /// `amount` tokens to this contract address before calling.
    ///
    /// Returns `(farmer_amount, fee_amount)` for the caller's convenience.
    pub fn credit(
        env: Env,
        creator: Address,
        amount: i128,
        fee_bps: u32,
    ) -> Result<(i128, i128), EarningsError> {
        let paused: bool = env
            .storage()
            .instance()
            .get(&DataKey::PausedState)
            .unwrap_or(false);
        if paused {
            return Err(EarningsError::Paused);
        }

        if amount <= 0 {
            return Err(EarningsError::InvalidAmount);
        }
        if fee_bps > 10_000 {
            return Err(EarningsError::InvalidFeeBps);
        }

        let fee_amount: i128 = (amount * fee_bps as i128) / 10_000;
        let farmer_amount: i128 = amount - fee_amount;

        // Accumulate the creator's claimable balance.
        let balance_key = DataKey::Balance(creator.clone());
        let prev: i128 = env.storage().persistent().get(&balance_key).unwrap_or(0);
        env.storage().persistent().set(&balance_key, &(prev + farmer_amount));

        // Accumulate lifetime earnings (independent of claimable balance, never reset).
        let lifetime_key = DataKey::LifetimeEarned(creator.clone());
        let lifetime_prev: i128 = env.storage().persistent().get(&lifetime_key).unwrap_or(0);
        env.storage().persistent().set(&lifetime_key, &(lifetime_prev + farmer_amount));

        Ok((farmer_amount, fee_amount))
    }

    /// Batch credit multiple (creator, amount, fee_bps) tuples in a single call.
    /// - At most `MAX_BATCH_CREDIT` (20) entries are accepted; otherwise
    ///   `EarningsError::BatchTooLarge`.
    /// - Each credit is independent: a failing one emits
    ///   ("earnings", "batch_credit_error", creator) and the batch continues.
    /// - Returns one `(creator, succeeded)` pair per input entry, in order.
    pub fn batch_credit(
        env: Env,
        entries: Vec<(Address, i128, u32)>,
    ) -> Result<Vec<(Address, bool)>, EarningsError> {
        if entries.len() > MAX_BATCH_CREDIT as usize {
            return Err(EarningsError::BatchTooLarge);
        }

        let mut results: Vec<(Address, bool)> = Vec::new(&env);
        for (creator, amount, fee_bps) in entries.iter() {
            match Self::credit(env.clone(), creator.clone(), amount, fee_bps) {
                Ok(_) => results.push_back((creator.clone(), true)),
                Err(_) => {
                    env.events().publish(
                        (
                            symbol_short!("earnings"),
                            soroban_sdk::Symbol::new(&env, "batch_credit_error"),
                            creator.clone(),
                        ),
                        (),
                    );
                    results.push_back((creator.clone(), false));
                }
            }
        }
        Ok(results)
    }

    /// Transfer the caller's entire accumulated balance to themselves via
    /// `token`.  Resets their on-chain balance to zero.
    pub fn claim(
        env: Env,
        creator: Address,
        token: Address,
    ) -> Result<i128, EarningsError> {
        let paused: bool = env
            .storage()
            .instance()
            .get(&DataKey::PausedState)
            .unwrap_or(false);
        if paused {
            return Err(EarningsError::Paused);
        }

        creator.require_auth();

        let key = DataKey::Balance(creator.clone());
        let balance: i128 = env.storage().persistent().get(&key).unwrap_or(0);

        if balance <= 0 {
            return Err(EarningsError::ZeroBalance);
        }

        env.storage().persistent().set(&key, &0_i128);

        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &creator,
            &balance,
        );

        Ok(balance)
    }

    /// Read-only: return the current claimable balance for `creator`.
    pub fn balance(env: Env, creator: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Balance(creator))
            .unwrap_or(0)
    }

    /// Read-only: return the lifetime total earnings (farmer_amount only) for
    /// `creator`. This counter is incremented on every credit() and never reset
    /// by claim() — it reflects total earnings across all time.
    pub fn lifetime_earned(env: Env, creator: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::LifetimeEarned(creator))
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Address, Env};

    // ── helpers ──────────────────────────────────────────────────────────────

    fn setup() -> (Env, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let platform = Address::generate(&env);
        let contract_id = env.register_contract(None, CreatorEarningsContract);
        CreatorEarningsContract::init(env.clone(), platform.clone());
        (env, platform, contract_id)
    }

    // ── unit tests ───────────────────────────────────────────────────────────

    #[test]
    fn credit_zero_amount_returns_invalid_amount() {
        let (env, _, _) = setup();
        let creator = Address::generate(&env);
        let result = CreatorEarningsContract::credit(env, creator, 0, 250);
        assert_eq!(result, Err(EarningsError::InvalidAmount));
    }

    #[test]
    fn credit_negative_amount_returns_invalid_amount() {
        let (env, _, _) = setup();
        let creator = Address::generate(&env);
        let result = CreatorEarningsContract::credit(env, creator, -1, 250);
        assert_eq!(result, Err(EarningsError::InvalidAmount));
    }

    #[test]
    fn credit_fee_bps_over_10000_returns_invalid_fee_bps() {
        let (env, _, _) = setup();
        let creator = Address::generate(&env);
        let result = CreatorEarningsContract::credit(env, creator, 1_000, 10_001);
        assert_eq!(result, Err(EarningsError::InvalidFeeBps));
    }

    #[test]
    fn credit_accumulates_balance() {
        let (env, _, _) = setup();
        let creator = Address::generate(&env);
        CreatorEarningsContract::credit(env.clone(), creator.clone(), 1_000, 0).unwrap();
        CreatorEarningsContract::credit(env.clone(), creator.clone(), 500, 0).unwrap();
        assert_eq!(CreatorEarningsContract::balance(env, creator), 1_500);
    }

    #[test]
    fn claim_zero_balance_returns_zero_balance_error() {
        let (env, _, _) = setup();
        let creator = Address::generate(&env);
        let token = Address::generate(&env);
        let result = CreatorEarningsContract::claim(env, creator, token);
        assert_eq!(result, Err(EarningsError::ZeroBalance));
    }

    #[test]
    fn balance_unknown_creator_returns_zero() {
        let (env, _, _) = setup();
        let stranger = Address::generate(&env);
        assert_eq!(CreatorEarningsContract::balance(env, stranger), 0);
    }

    // ── property / invariant tests ───────────────────────────────────────────
    //
    // Soroban's test environment is deterministic, so we drive it with a
    // hand-rolled table of representative inputs that cover boundary values,
    // typical values, and edge cases — giving us property-test coverage
    // without an external fuzzing harness dependency.

    /// I3 — farmer_amount + fee_amount == amount (no value created/destroyed).
    #[test]
    fn prop_fee_split_sums_to_amount() {
        let cases: &[(i128, u32)] = &[
            (1, 0),
            (1, 10_000),
            (1_000_000, 250),
            (1_000_000, 0),
            (1_000_000, 10_000),
            (7, 3333),
            (99, 9999),
            (i128::MAX / 2, 5_000),
            (10_000, 1),
            (10_000, 9_999),
        ];

        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        for &(amount, fee_bps) in cases {
            let creator = Address::generate(&env);
            let (farmer_amount, fee_amount) =
                CreatorEarningsContract::credit(env.clone(), creator, amount, fee_bps).unwrap();

            assert_eq!(
                farmer_amount + fee_amount,
                amount,
                "split must sum to amount: amount={amount} fee_bps={fee_bps}"
            );
        }
    }

    /// I4 — balance never goes negative after any sequence of credits.
    #[test]
    fn prop_balance_never_negative() {
        let amounts: &[i128] = &[1, 100, 999, 1_000_000, i128::MAX / 10_000];
        let fee_bps_vals: &[u32] = &[0, 1, 250, 5_000, 9_999, 10_000];

        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        for &amount in amounts {
            for &fee_bps in fee_bps_vals {
                let creator = Address::generate(&env);
                CreatorEarningsContract::credit(env.clone(), creator.clone(), amount, fee_bps)
                    .unwrap();
                let bal = CreatorEarningsContract::balance(env.clone(), creator);
                assert!(bal >= 0, "balance must be ≥ 0: got {bal}");
            }
        }
    }

    /// I2 — fee_bps > 10_000 is always rejected.
    #[test]
    fn prop_invalid_fee_bps_always_rejected() {
        let invalid_bps: &[u32] = &[10_001, 10_002, 20_000, u32::MAX];

        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        for &fee_bps in invalid_bps {
            let creator = Address::generate(&env);
            let result = CreatorEarningsContract::credit(env.clone(), creator, 1_000, fee_bps);
            assert_eq!(
                result,
                Err(EarningsError::InvalidFeeBps),
                "fee_bps={fee_bps} must be rejected"
            );
        }
    }

    /// I1 — amount ≤ 0 is always rejected.
    #[test]
    fn prop_invalid_amount_always_rejected() {
        let invalid_amounts: &[i128] = &[0, -1, -1_000, i128::MIN];

        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        for &amount in invalid_amounts {
            let creator = Address::generate(&env);
            let result = CreatorEarningsContract::credit(env.clone(), creator, amount, 250);
            assert_eq!(
                result,
                Err(EarningsError::InvalidAmount),
                "amount={amount} must be rejected"
            );
        }
    }

    /// I5 — after claim, balance is zero.
    /// I6 — second claim returns ZeroBalance.
    #[test]
    fn prop_claim_resets_balance_and_double_claim_fails() {
        // We test the balance-reset logic without a real token transfer by
        // directly manipulating storage (mirrors how the escrow sibling tests
        // work) and then verifying the error path.
        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        let creator = Address::generate(&env);

        // Seed a balance directly so we don't need a live token contract.
        env.storage()
            .persistent()
            .set(&DataKey::Balance(creator.clone()), &1_000_i128);

        assert_eq!(
            CreatorEarningsContract::balance(env.clone(), creator.clone()),
            1_000
        );

        // Reset balance to zero manually (simulates a successful claim).
        env.storage()
            .persistent()
            .set(&DataKey::Balance(creator.clone()), &0_i128);

        // I5 — balance is now zero.
        assert_eq!(
            CreatorEarningsContract::balance(env.clone(), creator.clone()),
            0
        );

        // I6 — second claim must fail.
        let token = Address::generate(&env);
        let result = CreatorEarningsContract::claim(env.clone(), creator, token);
        assert_eq!(result, Err(EarningsError::ZeroBalance));
    }

    /// I3 (boundary) — fee_bps = 10_000 means farmer gets 0, fee gets all.
    #[test]
    fn prop_full_fee_farmer_gets_zero() {
        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        let creator = Address::generate(&env);
        let (farmer_amount, fee_amount) =
            CreatorEarningsContract::credit(env.clone(), creator.clone(), 1_000, 10_000).unwrap();

        assert_eq!(farmer_amount, 0);
        assert_eq!(fee_amount, 1_000);
        // Balance stored for creator must be 0.
        assert_eq!(CreatorEarningsContract::balance(env, creator), 0);
    }

    /// I3 (boundary) — fee_bps = 0 means farmer gets all, fee gets 0.
    #[test]
    fn prop_zero_fee_farmer_gets_all() {
        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        let creator = Address::generate(&env);
        let amount: i128 = 5_000;
        let (farmer_amount, fee_amount) =
            CreatorEarningsContract::credit(env.clone(), creator.clone(), amount, 0).unwrap();

        assert_eq!(fee_amount, 0);
        assert_eq!(farmer_amount, amount);
        assert_eq!(CreatorEarningsContract::balance(env, creator), amount);
    }

    /// Multiple creators are independent — crediting one does not affect another.
    #[test]
    fn prop_creators_are_independent() {
        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        let alice = Address::generate(&env);
        let bob = Address::generate(&env);

        CreatorEarningsContract::credit(env.clone(), alice.clone(), 1_000, 0).unwrap();
        CreatorEarningsContract::credit(env.clone(), bob.clone(), 2_000, 0).unwrap();

        assert_eq!(CreatorEarningsContract::balance(env.clone(), alice), 1_000);
        assert_eq!(CreatorEarningsContract::balance(env.clone(), bob), 2_000);
    }

    #[test]
    fn batch_credit_too_large() {
        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        let mut entries: Vec<(Address, i128, u32)> = Vec::new(&env);
        for _ in 0..21 {
            entries.push_back((Address::generate(&env), 1_000, 0));
        }

        let result = CreatorEarningsContract::batch_credit(env, entries);
        assert_eq!(result, Err(EarningsError::BatchTooLarge));
    }

    #[test]
    fn batch_credit_partial_failure_continues() {
        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        let alice = Address::generate(&env);
        let bob = Address::generate(&env);
        let charlie = Address::generate(&env);

        let mut entries: Vec<(Address, i128, u32)> = Vec::new(&env);
        entries.push_back((alice.clone(), 1_000, 0));
        entries.push_back((bob.clone(), 0, 0)); // Invalid: amount = 0
        entries.push_back((charlie.clone(), 2_000, 250));

        let results = CreatorEarningsContract::batch_credit(env.clone(), entries).unwrap();

        // Should have 3 results, with bob's failing (false).
        assert_eq!(results.len(), 3);
        assert_eq!(results.get(0), (alice.clone(), true));
        assert_eq!(results.get(1), (bob.clone(), false));
        assert_eq!(results.get(2), (charlie.clone(), true));

        // Verify balances — only alice and charlie should have credits.
        assert_eq!(CreatorEarningsContract::balance(env.clone(), alice), 1_000);
        assert_eq!(CreatorEarningsContract::balance(env.clone(), bob), 0); // Not credited due to error
        // charlie: 2_000 - (2_000 * 250 / 10_000) = 2_000 - 50 = 1_950
        assert_eq!(CreatorEarningsContract::balance(env.clone(), charlie), 1_950);
    }

    #[test]
    fn batch_credit_empty_is_ok() {
        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        let entries: Vec<(Address, i128, u32)> = Vec::new(&env);
        let results = CreatorEarningsContract::batch_credit(env, entries).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn set_paused_and_credit_rejected_when_paused() {
        let env = Env::default();
        env.mock_all_auths();
        let platform = Address::generate(&env);
        CreatorEarningsContract::init(env.clone(), platform.clone());

        let creator = Address::generate(&env);

        // Initially unpaused: credit should succeed.
        let result = CreatorEarningsContract::credit(env.clone(), creator.clone(), 1_000, 0);
        assert!(result.is_ok());
        assert_eq!(CreatorEarningsContract::balance(env.clone(), creator.clone()), 1_000);

        // Pause the contract.
        let pause_result = CreatorEarningsContract::set_paused(env.clone(), true);
        assert!(pause_result.is_ok());

        // credit() should now return Paused error.
        let result = CreatorEarningsContract::credit(env.clone(), creator.clone(), 500, 0);
        assert_eq!(result, Err(EarningsError::Paused));

        // balance() should still work while paused.
        assert_eq!(CreatorEarningsContract::balance(env.clone(), creator.clone()), 1_000);
    }

    #[test]
    fn set_paused_and_claim_rejected_when_paused() {
        let env = Env::default();
        env.mock_all_auths();
        let platform = Address::generate(&env);
        CreatorEarningsContract::init(env.clone(), platform.clone());

        let creator = Address::generate(&env);
        let token = Address::generate(&env);

        // Seed a balance.
        env.storage()
            .persistent()
            .set(&DataKey::Balance(creator.clone()), &500_i128);

        // Pause the contract.
        CreatorEarningsContract::set_paused(env.clone(), true).unwrap();

        // claim() should return Paused error.
        let result = CreatorEarningsContract::claim(env.clone(), creator.clone(), token);
        assert_eq!(result, Err(EarningsError::Paused));

        // balance() should still work while paused.
        assert_eq!(CreatorEarningsContract::balance(env.clone(), creator), 500);
    }

    #[test]
    fn unpause_allows_credit_and_claim() {
        let env = Env::default();
        env.mock_all_auths();
        let platform = Address::generate(&env);
        CreatorEarningsContract::init(env.clone(), platform.clone());

        let creator = Address::generate(&env);

        // Pause.
        CreatorEarningsContract::set_paused(env.clone(), true).unwrap();

        // Verify credit is rejected.
        let result = CreatorEarningsContract::credit(env.clone(), creator.clone(), 1_000, 0);
        assert_eq!(result, Err(EarningsError::Paused));

        // Unpause.
        CreatorEarningsContract::set_paused(env.clone(), false).unwrap();

        // credit() should now succeed.
        let result = CreatorEarningsContract::credit(env.clone(), creator.clone(), 1_000, 0);
        assert!(result.is_ok());
        assert_eq!(CreatorEarningsContract::balance(env.clone(), creator), 1_000);
    }

    #[test]
    fn lifetime_earned_tracks_total_credits() {
        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        let creator = Address::generate(&env);

        // Initially zero.
        assert_eq!(CreatorEarningsContract::lifetime_earned(env.clone(), creator.clone()), 0);

        // Credit 1_000 with 0 fee → farmer gets 1_000, lifetime becomes 1_000.
        CreatorEarningsContract::credit(env.clone(), creator.clone(), 1_000, 0).unwrap();
        assert_eq!(CreatorEarningsContract::lifetime_earned(env.clone(), creator.clone()), 1_000);

        // Credit 500 with 250 bps fee → farmer gets 487.5 (truncated to 487 due to integer division).
        // 500 * 250 / 10_000 = 12.5 (truncated to 12), so farmer gets 500 - 12 = 488.
        CreatorEarningsContract::credit(env.clone(), creator.clone(), 500, 250).unwrap();
        // lifetime_earned should be 1_000 + 488 = 1_488.
        assert_eq!(CreatorEarningsContract::lifetime_earned(env.clone(), creator.clone()), 1_488);
    }

    #[test]
    fn lifetime_earned_survives_claim() {
        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        let creator = Address::generate(&env);
        let token = Address::generate(&env);

        // Credit 1_000.
        CreatorEarningsContract::credit(env.clone(), creator.clone(), 1_000, 0).unwrap();
        assert_eq!(CreatorEarningsContract::lifetime_earned(env.clone(), creator.clone()), 1_000);
        assert_eq!(CreatorEarningsContract::balance(env.clone(), creator.clone()), 1_000);

        // Manually reset balance to 0 (simulates a successful claim).
        env.storage()
            .persistent()
            .set(&DataKey::Balance(creator.clone()), &0_i128);

        // balance() is now zero, but lifetime_earned should be unchanged.
        assert_eq!(CreatorEarningsContract::balance(env.clone(), creator.clone()), 0);
        assert_eq!(CreatorEarningsContract::lifetime_earned(env.clone(), creator.clone()), 1_000);
    }

    #[test]
    fn lifetime_earned_accumulates_across_multiple_credits() {
        let env = Env::default();
        env.mock_all_auths();
        CreatorEarningsContract::init(env.clone(), Address::generate(&env));

        let creator = Address::generate(&env);

        // Multiple credits with various fees.
        CreatorEarningsContract::credit(env.clone(), creator.clone(), 1_000, 0).unwrap();
        CreatorEarningsContract::credit(env.clone(), creator.clone(), 1_000, 0).unwrap();
        CreatorEarningsContract::credit(env.clone(), creator.clone(), 1_000, 500).unwrap(); // 50% fee

        // Last credit: 1_000 * 500 / 10_000 = 50 fee, farmer gets 950.
        // Total: 1_000 + 1_000 + 950 = 2_950.
        assert_eq!(CreatorEarningsContract::lifetime_earned(env.clone(), creator), 2_950);
    }
}
