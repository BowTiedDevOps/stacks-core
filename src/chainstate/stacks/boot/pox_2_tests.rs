use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::TryFrom;
use std::convert::TryInto;

use crate::address::AddressHashMode;
use crate::chainstate::burn::BlockSnapshot;
use crate::chainstate::burn::ConsensusHash;
use crate::chainstate::stacks::address::PoxAddress;
use crate::chainstate::stacks::boot::{
    BOOT_CODE_COST_VOTING_TESTNET as BOOT_CODE_COST_VOTING, BOOT_CODE_POX_TESTNET,
};
use crate::chainstate::stacks::db::{
    MinerPaymentSchedule, StacksChainState, StacksHeaderInfo, MINER_REWARD_MATURITY,
};
use crate::chainstate::stacks::index::marf::MarfConnection;
use crate::chainstate::stacks::index::MarfTrieId;
use crate::chainstate::stacks::*;
use crate::clarity_vm::database::marf::MarfedKV;
use crate::clarity_vm::database::HeadersDBConn;
use crate::core::*;
use crate::util_lib::db::{DBConn, FromRow};
use clarity::vm::contexts::OwnedEnvironment;
use clarity::vm::contracts::Contract;
use clarity::vm::costs::CostOverflowingMath;
use clarity::vm::database::*;
use clarity::vm::errors::{
    CheckErrors, Error, IncomparableError, InterpreterError, InterpreterResult, RuntimeErrorType,
};
use clarity::vm::eval;
use clarity::vm::representations::SymbolicExpression;
use clarity::vm::tests::{execute, is_committed, is_err_code, symbols_from_values};
use clarity::vm::types::Value::Response;
use clarity::vm::types::{
    OptionalData, PrincipalData, QualifiedContractIdentifier, ResponseData,
    StacksAddressExtensions, StandardPrincipalData, TupleData, TupleTypeSignature, TypeSignature,
    Value, NONE,
};
use stacks_common::util::hash::to_hex;
use stacks_common::util::hash::{Sha256Sum, Sha512Trunc256Sum};

use crate::net::test::TestPeer;
use crate::util_lib::boot::boot_code_id;
use crate::{
    burnchains::Burnchain,
    chainstate::{
        burn::db::sortdb::SortitionDB,
        stacks::{events::TransactionOrigin, tests::make_coinbase},
    },
    clarity_vm::{clarity::ClarityBlockConnection, database::marf::WritableMarfStore},
    net::test::TestEventObserver,
};
use stacks_common::types::chainstate::{
    BlockHeaderHash, BurnchainHeaderHash, StacksAddress, StacksBlockId, VRFSeed,
};

use super::{test::*, RawRewardSetEntry};
use crate::clarity_vm::clarity::Error as ClarityError;

use crate::chainstate::burn::operations::*;
use clarity::vm::clarity::ClarityConnection;
use clarity::vm::costs::LimitedCostTracker;
use core::*;

const USTX_PER_HOLDER: u128 = 1_000_000;

/// Return the BlockSnapshot for the latest sortition in the provided
///  SortitionDB option-reference. Panics on any errors.
fn get_tip(sortdb: Option<&SortitionDB>) -> BlockSnapshot {
    SortitionDB::get_canonical_burn_chain_tip(&sortdb.unwrap().conn()).unwrap()
}

/// Get the reward set entries if evaluated at the given StacksBlock
pub fn get_reward_set_entries_at(
    peer: &mut TestPeer,
    tip: &StacksBlockId,
    at_burn_ht: u64,
) -> Vec<RawRewardSetEntry> {
    let burnchain = peer.config.burnchain.clone();
    with_sortdb(peer, |ref mut c, ref sortdb| {
        get_reward_set_entries_at_block(c, &burnchain, sortdb, tip, at_burn_ht).unwrap()
    })
}

/// Get the reward set entries if evaluated at the given StacksBlock
///  in order of index in the reward-cycle-address-list map
pub fn get_reward_set_entries_index_order_at(
    peer: &mut TestPeer,
    tip: &StacksBlockId,
    at_burn_ht: u64,
) -> Vec<RawRewardSetEntry> {
    let burnchain = peer.config.burnchain.clone();
    with_sortdb(peer, |ref mut c, ref sortdb| {
        c.get_reward_addresses(&burnchain, sortdb, at_burn_ht, tip)
            .unwrap()
    })
}

/// Get the STXBalance for `account` at the given chaintip
pub fn get_stx_account_at(
    peer: &mut TestPeer,
    tip: &StacksBlockId,
    account: &PrincipalData,
) -> STXBalance {
    with_clarity_db_ro(peer, tip, |db| db.get_account_stx_balance(account))
}

/// Get the STXBalance for `account` at the given chaintip
pub fn get_stacking_state_pox_2(
    peer: &mut TestPeer,
    tip: &StacksBlockId,
    account: &PrincipalData,
) -> Option<Value> {
    with_clarity_db_ro(peer, tip, |db| {
        let lookup_tuple = Value::Tuple(
            TupleData::from_data(vec![("stacker".into(), account.clone().into())]).unwrap(),
        );
        db.fetch_entry_unknown_descriptor(
            &boot_code_id(boot::POX_2_NAME, false),
            "stacking-state",
            &lookup_tuple,
        )
        .unwrap()
        .expect_optional()
    })
}

/// Perform `check_stacker_link_invariants` on cycles [first_cycle_number, max_cycle_number]
pub fn check_all_stacker_link_invariants(
    peer: &mut TestPeer,
    tip: &StacksBlockId,
    first_cycle_number: u64,
    max_cycle_number: u64,
) {
    // if PoX-2 hasn't published yet, just return.
    let epoch = with_clarity_db_ro(peer, tip, |db| db.get_clarity_epoch_version());
    if epoch < StacksEpochId::Epoch21 {
        eprintln!("Skipping invariant checks when PoX-2 has not published yet");
        return;
    } else {
        eprintln!("Invariants being checked");
    }

    info!("Invoked check all"; "tip" => %tip, "first" => first_cycle_number, "last" => max_cycle_number);
    for cycle in first_cycle_number..(max_cycle_number + 1) {
        check_stacker_link_invariants(peer, tip, cycle);
    }
}

pub struct StackingStateCheckData {
    pox_addr: PoxAddress,
    /// this is a map from reward cycle number to the value in reward-set-indexes
    cycle_indexes: HashMap<u128, u128>,
    first_cycle: u128,
    lock_period: u128,
}

/// Check the stacking-state invariants of `stacker`
/// Mostly that all `stacking-state.reward-set-indexes` match the index of their reward cycle entries
pub fn check_stacking_state_invariants(
    peer: &mut TestPeer,
    tip: &StacksBlockId,
    stacker: &PrincipalData,
    expect_indexes: bool,
) -> StackingStateCheckData {
    let account_state = with_clarity_db_ro(peer, tip, |db| {
        db.get_stx_balance_snapshot(stacker)
            .canonical_balance_repr()
    });

    let stacking_state_entry = get_stacking_state_pox_2(peer, tip, stacker)
        .expect("Invariant violated: reward-cycle entry has stacker field set, but not present in stacker-state")
        .expect_tuple();
    let first_cycle = stacking_state_entry
        .get("first-reward-cycle")
        .unwrap()
        .clone()
        .expect_u128();
    let lock_period = stacking_state_entry
        .get("lock-period")
        .unwrap()
        .clone()
        .expect_u128();
    let pox_addr = stacking_state_entry.get("pox-addr").unwrap();
    let pox_addr =
        PoxAddress::try_from_pox_tuple(peer.config.burnchain.is_mainnet(), pox_addr).unwrap();

    let reward_indexes: Vec<u128> = stacking_state_entry
        .get_owned("reward-set-indexes")
        .unwrap()
        .expect_list()
        .into_iter()
        .map(|x| x.expect_u128())
        .collect();

    let stacking_state_unlock_ht = peer
        .config
        .burnchain
        .reward_cycle_to_block_height((first_cycle + lock_period) as u64);

    assert_eq!(
        account_state.unlock_height() + 1,
        stacking_state_unlock_ht,
        "Invariant violated: stacking-state and account state have different unlock heights"
    );

    let mut cycle_indexes = HashMap::new();

    if reward_indexes.len() > 0 || expect_indexes {
        assert_eq!(
            reward_indexes.len() as u128,
            lock_period,
            "Invariant violated: lock-period should be equal to the reward indexes length"
        );

        for i in 0..lock_period {
            let cycle_checked = first_cycle + i;
            let reward_index = reward_indexes[i as usize];

            let entry_key = Value::from(
                TupleData::from_data(vec![
                    ("reward-cycle".into(), Value::UInt(cycle_checked.into())),
                    ("index".into(), Value::UInt(reward_index)),
                ])
                .unwrap(),
            );
            let entry_value = with_clarity_db_ro(peer, tip, |db| {
                db.fetch_entry_unknown_descriptor(
                    &boot_code_id(boot::POX_2_NAME, false),
                    "reward-cycle-pox-address-list",
                    &entry_key
                )
                    .unwrap()
                    .expect_optional()
                    .expect("Invariant violated: stacking-state.reward-set-indexes pointed at a non-existent entry")
                    .expect_tuple()
            });

            let entry_stacker = entry_value.get("stacker")
                .unwrap()
                .clone()
                .expect_optional()
                .expect("Invariant violated: stacking-state.reward-set-indexes pointed at an entry without a stacker set")
                .expect_principal();

            assert_eq!(
                &entry_stacker, stacker,
                "Invariant violated: reward-set-index points to different stacker's entry"
            );

            let entry_pox_addr = entry_value.get_owned("pox-addr").unwrap();
            let entry_pox_addr =
                PoxAddress::try_from_pox_tuple(peer.config.burnchain.is_mainnet(), &entry_pox_addr)
                    .unwrap();

            assert_eq!(
                &entry_pox_addr, &pox_addr,
                "Invariant violated: linked reward set entry has a different PoX address"
            );

            cycle_indexes.insert(cycle_checked, reward_index);
        }
    }

    StackingStateCheckData {
        cycle_indexes,
        pox_addr,
        first_cycle,
        lock_period,
    }
}

/// Check that:
///  (1) `reward-cycle-pox-address-list.stacker` points to a real `stacking-state`
///  (2) `stacking-state.reward-set-indexes` matches the index of that `reward-cycle-pox-address-list`
///  (3) all `stacking-state.reward-set-indexes` match the index of their reward cycle entries
///  (4) `reward-cycle-total-stacked` is equal to the sum of all entries
///  (5) `stacking-state.pox-addr` matches `reward-cycle-pox-address-list.pox-addr`
pub fn check_stacker_link_invariants(peer: &mut TestPeer, tip: &StacksBlockId, cycle_number: u64) {
    let current_burn_height = StacksChainState::get_stacks_block_header_info_by_index_block_hash(
        peer.chainstate().db(),
        tip,
    )
    .unwrap()
    .unwrap()
    .burn_header_height;
    let tip_cycle = peer
        .config
        .burnchain
        .block_height_to_reward_cycle(current_burn_height.into())
        .unwrap();
    let cycle_start = peer
        .config
        .burnchain
        .reward_cycle_to_block_height(cycle_number);
    let reward_set_entries = get_reward_set_entries_index_order_at(peer, tip, cycle_start);
    let mut checked_total = 0;
    for (actual_index, entry) in reward_set_entries.iter().enumerate() {
        checked_total += entry.amount_stacked;
        if let Some(stacker) = &entry.stacker {
            if tip_cycle > cycle_start {
                // if the checked cycle is before the tip's cycle,
                // the reward-set-entrie's stacker links are no longer necessarily valid
                // (because the reward cycles for those entries has passed)
                // so we continue here to skip the stacker reference checks
                continue;
            }

            let StackingStateCheckData {
                pox_addr,
                cycle_indexes,
                ..
            } = check_stacking_state_invariants(peer, tip, stacker, true);

            assert_eq!(&entry.reward_address, &pox_addr, "Invariant violated: reward-cycle entry has a different PoX addr than in stacker-state");
            assert_eq!(cycle_indexes.get(&(cycle_number as u128)).cloned().unwrap(), actual_index as u128, "Invariant violated: stacking-state.reward-set-indexes entry at cycle_number must point to this stacker's entry");
        }
    }
    let expected_total = get_reward_cycle_total(peer, tip, cycle_number);
    assert_eq!(
        u128::try_from(checked_total).unwrap(),
        expected_total,
        "Invariant violated: total reward cycle amount does not equal sum of reward set"
    );
}

/// Get the `cycle_number`'s total stacked amount at the given chaintip
pub fn get_reward_cycle_total(peer: &mut TestPeer, tip: &StacksBlockId, cycle_number: u64) -> u128 {
    with_clarity_db_ro(peer, tip, |db| {
        let total_stacked_key = TupleData::from_data(vec![(
            "reward-cycle".into(),
            Value::UInt(cycle_number.into()),
        )])
        .unwrap()
        .into();
        db.fetch_entry_unknown_descriptor(
            &boot_code_id(boot::POX_2_NAME, false),
            "reward-cycle-total-stacked",
            &total_stacked_key,
        )
        .map(|v| {
            v.expect_optional()
                .map(|v| {
                    v.expect_tuple()
                        .get_owned("total-ustx")
                        .expect("Malformed tuple returned by PoX contract")
                        .expect_u128()
                })
                // if no entry yet, return 0
                .unwrap_or(0)
        })
        // if the map doesn't exist yet, return 0
        .unwrap_or(0)
    })
}

/// Get the `partial-stacked-by-cycle` entry at a given chain tip
pub fn get_partial_stacked(
    peer: &mut TestPeer,
    tip: &StacksBlockId,
    pox_addr: &Value,
    cycle_number: u64,
    sender: &PrincipalData,
) -> u128 {
    with_clarity_db_ro(peer, tip, |db| {
        let key = TupleData::from_data(vec![
            ("pox-addr".into(), pox_addr.clone()),
            ("reward-cycle".into(), Value::UInt(cycle_number.into())),
            ("sender".into(), Value::from(sender.clone())),
        ])
        .unwrap()
        .into();
        db.fetch_entry_unknown_descriptor(
            &boot_code_id(boot::POX_2_NAME, false),
            "partial-stacked-by-cycle",
            &key,
        )
        .map(|v| {
            v.expect_optional()
                .expect("Expected fetch_entry to return a value")
        })
        .unwrap()
        .expect_tuple()
        .get_owned("stacked-amount")
        .expect("Malformed tuple returned by PoX contract")
        .expect_u128()
    })
}

/// Allows you to do something read-only with the ClarityDB at the given chaintip
pub fn with_clarity_db_ro<F, R>(peer: &mut TestPeer, tip: &StacksBlockId, todo: F) -> R
where
    F: FnOnce(&mut ClarityDatabase) -> R,
{
    with_sortdb(peer, |ref mut c, ref sortdb| {
        let headers_db = HeadersDBConn(c.state_index.sqlite_conn());
        let burn_db = sortdb.index_conn();
        let mut read_only_clar = c
            .clarity_state
            .read_only_connection(tip, &headers_db, &burn_db);
        read_only_clar.with_clarity_db_readonly(todo)
    })
}

/// In this test case, two Stackers, Alice and Bob stack and interact with the
///  PoX v1 contract and PoX v2 contract across the epoch transition.
///
/// Alice: stacks via PoX v1 for 4 cycles. The third of these cycles occurs after
///        the PoX v1 -> v2 transition, and so Alice gets "early unlocked".
///        After the early unlock, Alice re-stacks in PoX v2
///        Alice tries to stack again via PoX v1, which is allowed by the contract,
///        but forbidden by the VM (because PoX has transitioned to v2)
/// Bob:   stacks via PoX v2 for 6 cycles. He attempted to stack via PoX v1 as well,
///        but is forbidden because he has already placed an account lock via PoX v2.
///
#[test]
fn test_simple_pox_lockup_transition_pox_2() {
    // this is the number of blocks after the first sortition any V1
    // PoX locks will automatically unlock at.
    let AUTO_UNLOCK_HEIGHT = 12;
    let EXPECTED_FIRST_V2_CYCLE = 8;
    // the sim environment produces 25 empty sortitions before
    //  tenures start being tracked.
    let EMPTY_SORTITIONS = 25;

    let mut burnchain = Burnchain::default_unittest(
        0,
        &BurnchainHeaderHash::from_hex(BITCOIN_REGTEST_FIRST_BLOCK_HASH).unwrap(),
    );
    burnchain.pox_constants.reward_cycle_length = 5;
    burnchain.pox_constants.prepare_length = 2;
    burnchain.pox_constants.anchor_threshold = 1;
    burnchain.pox_constants.v1_unlock_height = AUTO_UNLOCK_HEIGHT + EMPTY_SORTITIONS;

    let first_v2_cycle = burnchain
        .block_height_to_reward_cycle(burnchain.pox_constants.v1_unlock_height as u64)
        .unwrap()
        + 1;

    assert_eq!(first_v2_cycle, EXPECTED_FIRST_V2_CYCLE);

    eprintln!("First v2 cycle = {}", first_v2_cycle);

    let epochs = StacksEpoch::all(0, 0, EMPTY_SORTITIONS as u64 + 10);

    let observer = TestEventObserver::new();

    let (mut peer, mut keys) = instantiate_pox_peer_with_epoch(
        &burnchain,
        "test_simple_pox_lockup_transition_pox_2",
        6002,
        Some(epochs.clone()),
        Some(&observer),
    );

    peer.config.check_pox_invariants =
        Some((EXPECTED_FIRST_V2_CYCLE, EXPECTED_FIRST_V2_CYCLE + 10));

    let num_blocks = 35;

    let alice = keys.pop().unwrap();
    let bob = keys.pop().unwrap();
    let charlie = keys.pop().unwrap();

    let EXPECTED_ALICE_FIRST_REWARD_CYCLE = 6;

    let mut coinbase_nonce = 0;

    // these checks are very repetitive
    let reward_cycle_checks = |tip_index_block| {
        let tip_burn_block_height = get_par_burn_block_height(peer.chainstate(), &tip_index_block);
        let cur_reward_cycle = burnchain
            .block_height_to_reward_cycle(tip_burn_block_height)
            .unwrap() as u128;
        let (min_ustx, reward_addrs, total_stacked) =
            with_sortdb(&mut peer, |ref mut c, ref sortdb| {
                (
                    c.get_stacking_minimum(sortdb, &tip_index_block).unwrap(),
                    get_reward_addresses_with_par_tip(c, &burnchain, sortdb, &tip_index_block)
                        .unwrap(),
                    c.test_get_total_ustx_stacked(sortdb, &tip_index_block, cur_reward_cycle)
                        .unwrap(),
                )
            });

        eprintln!(
            "\nreward cycle: {}\nmin-uSTX: {}\naddrs: {:?}\ntotal-stacked: {}\n",
            cur_reward_cycle, min_ustx, &reward_addrs, total_stacked
        );

        if cur_reward_cycle < EXPECTED_ALICE_FIRST_REWARD_CYCLE {
            // no reward addresses yet
            assert_eq!(reward_addrs.len(), 0);
        } else if cur_reward_cycle < EXPECTED_FIRST_V2_CYCLE as u128 {
            // After the start of Alice's first cycle, but before the first V2 cycle,
            //  Alice is the only Stacker, so check that.
            let (amount_ustx, pox_addr, lock_period, first_reward_cycle) =
                get_stacker_info(&mut peer, &key_to_stacks_addr(&alice).into()).unwrap();
            eprintln!("\nAlice: {} uSTX stacked for {} cycle(s); addr is {:?}; first reward cycle is {}\n", amount_ustx, lock_period, &pox_addr, first_reward_cycle);

            // one reward address, and it's Alice's
            // either way, there's a single reward address
            assert_eq!(reward_addrs.len(), 1);
            assert_eq!(
                (reward_addrs[0].0).version(),
                AddressHashMode::SerializeP2PKH as u8
            );
            assert_eq!(
                (reward_addrs[0].0).hash160(),
                key_to_stacks_addr(&alice).bytes
            );
            assert_eq!(reward_addrs[0].1, 1024 * POX_THRESHOLD_STEPS_USTX);
        } else {
            // v2 reward cycles have begun, so reward addrs should be read from PoX2 which is Bob + Alice
            assert_eq!(reward_addrs.len(), 2);
            assert_eq!(
                (reward_addrs[0].0).version(),
                AddressHashMode::SerializeP2PKH as u8
            );
            assert_eq!(
                (reward_addrs[0].0).hash160(),
                key_to_stacks_addr(&bob).bytes
            );
            assert_eq!(reward_addrs[0].1, 512 * POX_THRESHOLD_STEPS_USTX);

            assert_eq!(
                (reward_addrs[1].0).version(),
                AddressHashMode::SerializeP2PKH as u8
            );
            assert_eq!(
                (reward_addrs[1].0).hash160(),
                key_to_stacks_addr(&alice).bytes
            );
            assert_eq!(reward_addrs[1].1, 512 * POX_THRESHOLD_STEPS_USTX);
        }
    };

    // our "tenure counter" is now at 0
    let tip = get_tip(peer.sortdb.as_ref());
    assert_eq!(tip.block_height, 0 + EMPTY_SORTITIONS as u64);

    // first tenure is empty
    peer.tenure_with_txs(&[], &mut coinbase_nonce);

    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, 1024 * POX_THRESHOLD_STEPS_USTX);

    let alice_account = get_account(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(
        alice_account.stx_balance.amount_unlocked(),
        1024 * POX_THRESHOLD_STEPS_USTX
    );
    assert_eq!(alice_account.stx_balance.amount_locked(), 0);
    assert_eq!(alice_account.stx_balance.unlock_height(), 0);

    // next tenure include Alice's lockup
    let tip = get_tip(peer.sortdb.as_ref());
    let alice_lockup = make_pox_lockup(
        &alice,
        0,
        1024 * POX_THRESHOLD_STEPS_USTX,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        4,
        tip.block_height,
    );

    // our "tenure counter" is now at 1
    assert_eq!(tip.block_height, 1 + EMPTY_SORTITIONS as u64);

    let tip_index_block = peer.tenure_with_txs(&[alice_lockup], &mut coinbase_nonce);

    // check the stacking minimum
    let total_liquid_ustx = get_liquid_ustx(&mut peer);
    let min_ustx = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
        chainstate.get_stacking_minimum(sortdb, &tip_index_block)
    })
    .unwrap();
    assert_eq!(
        min_ustx,
        total_liquid_ustx / POX_TESTNET_STACKING_THRESHOLD_25
    );

    // no reward addresses
    let reward_addrs = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
        get_reward_addresses_with_par_tip(chainstate, &burnchain, sortdb, &tip_index_block)
    })
    .unwrap();
    assert_eq!(reward_addrs.len(), 0);

    // check the first reward cycle when Alice's tokens get stacked
    let tip_burn_block_height = get_par_burn_block_height(peer.chainstate(), &tip_index_block);
    let alice_first_reward_cycle = 1 + burnchain
        .block_height_to_reward_cycle(tip_burn_block_height)
        .unwrap() as u128;

    assert_eq!(alice_first_reward_cycle, EXPECTED_ALICE_FIRST_REWARD_CYCLE);

    // alice locked, so balance should be 0
    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, 0);

    // produce blocks until immediately before the epoch switch (7 more blocks to block height 35)

    for _i in 0..7 {
        peer.tenure_with_txs(&[], &mut coinbase_nonce);

        // alice is still locked, balance should be 0
        let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
        assert_eq!(alice_balance, 0);
    }

    // Have Charlie try to use the PoX2 contract. This transaction
    //  should be accepted (checked via the tx receipt). Also, importantly,
    //  the cost tracker should assign costs to Charlie's transaction.
    //  This is also checked by the transaction receipt.
    let tip = get_tip(peer.sortdb.as_ref());

    // our "tenure counter" is now at 9
    assert_eq!(tip.block_height, 9 + EMPTY_SORTITIONS as u64);

    let test = make_pox_2_contract_call(
        &charlie,
        0,
        "delegate-stx",
        vec![
            Value::UInt(1_000_000),
            PrincipalData::from(key_to_stacks_addr(&charlie)).into(),
            Value::none(),
            Value::none(),
        ],
    );
    peer.tenure_with_txs(&[test], &mut coinbase_nonce);

    // alice is still locked, balance should be 0
    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, 0);

    // in the next tenure, PoX 2 should now exist.
    // Lets have Bob lock up for v2
    // this will lock for cycles 8, 9, 10, and 11
    //  the first v2 cycle will be 8
    let tip = get_tip(peer.sortdb.as_ref());

    let bob_lockup = make_pox_2_lockup(
        &bob,
        0,
        512 * POX_THRESHOLD_STEPS_USTX,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&bob).bytes,
        6,
        tip.block_height,
    );

    // our "tenure counter" is now at 10
    assert_eq!(tip.block_height, 10 + EMPTY_SORTITIONS as u64);

    let block_id = peer.tenure_with_txs(&[bob_lockup], &mut coinbase_nonce);

    // alice is still locked, balance should be 0
    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, 0);

    // Now, Bob tries to lock in PoX v1 too, but it shouldn't work!
    let tip = get_tip(peer.sortdb.as_ref());

    let bob_lockup = make_pox_lockup(
        &bob,
        1,
        512 * POX_THRESHOLD_STEPS_USTX,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&bob).bytes,
        4,
        tip.block_height,
    );

    // our "tenure counter" is now at 11
    assert_eq!(tip.block_height, 11 + EMPTY_SORTITIONS as u64);
    let block_id = peer.tenure_with_txs(&[bob_lockup], &mut coinbase_nonce);

    // our "tenure counter" is now at 12
    let tip = get_tip(peer.sortdb.as_ref());
    assert_eq!(tip.block_height, 12 + EMPTY_SORTITIONS as u64);
    // One more empty tenure to reach the unlock height
    let block_id = peer.tenure_with_txs(&[], &mut coinbase_nonce);

    // Auto unlock height is reached, Alice balance should be unlocked
    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, 1024 * POX_THRESHOLD_STEPS_USTX);

    // At this point, the auto unlock height for v1 accounts should be reached.
    //  let Alice stack in PoX v2
    let tip = get_tip(peer.sortdb.as_ref());

    // our "tenure counter" is now at 13
    assert_eq!(tip.block_height, 13 + EMPTY_SORTITIONS as u64);

    let alice_lockup = make_pox_2_lockup(
        &alice,
        1,
        512 * POX_THRESHOLD_STEPS_USTX,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        12,
        tip.block_height,
    );
    peer.tenure_with_txs(&[alice_lockup], &mut coinbase_nonce);

    // Alice locked half her balance in PoX 2
    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, 512 * POX_THRESHOLD_STEPS_USTX);

    // now, let's roll the chain forward until Alice *would* have unlocked in v1 anyways.
    //  that's block height 31, so play 27 empty blocks

    for _i in 0..17 {
        peer.tenure_with_txs(&[], &mut coinbase_nonce);
        // at this point, alice's balance should always include this half lockup
        assert_eq!(alice_balance, 512 * POX_THRESHOLD_STEPS_USTX);
    }

    let tip = get_tip(peer.sortdb.as_ref());

    // our "tenure counter" is now at 31
    assert_eq!(tip.block_height, 31 + EMPTY_SORTITIONS as u64);

    let alice_lockup = make_pox_lockup(
        &alice,
        2,
        512 * POX_THRESHOLD_STEPS_USTX,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        12,
        tip.block_height,
    );
    let block_id = peer.tenure_with_txs(&[alice_lockup], &mut coinbase_nonce);

    assert_eq!(alice_balance, 512 * POX_THRESHOLD_STEPS_USTX);

    // now let's check some tx receipts

    let alice_address = key_to_stacks_addr(&alice);
    let bob_address = key_to_stacks_addr(&bob);
    let blocks = observer.get_blocks();

    let mut alice_txs = HashMap::new();
    let mut bob_txs = HashMap::new();
    let mut charlie_txs = HashMap::new();

    eprintln!("Alice addr: {}", alice_address);
    eprintln!("Bob addr: {}", bob_address);

    let mut tested_charlie = false;

    for b in blocks.into_iter() {
        for r in b.receipts.into_iter() {
            if let TransactionOrigin::Stacks(ref t) = r.transaction {
                let addr = t.auth.origin().address_testnet();
                eprintln!("TX addr: {}", addr);
                if addr == alice_address {
                    alice_txs.insert(t.auth.get_origin_nonce(), r);
                } else if addr == bob_address {
                    bob_txs.insert(t.auth.get_origin_nonce(), r);
                } else if addr == key_to_stacks_addr(&charlie) {
                    assert!(
                        r.execution_cost != ExecutionCost::zero(),
                        "Execution cost is not zero!"
                    );
                    charlie_txs.insert(t.auth.get_origin_nonce(), r);

                    tested_charlie = true;
                }
            }
        }
    }

    assert!(tested_charlie, "Charlie TX must be tested");
    // Alice should have three accepted transactions:
    //  TX0 -> Alice's initial lockup in PoX 1
    //  TX1 -> Alice's PoX 2 lockup
    //  TX2 -> Alice's attempt to lock again in PoX 1 -- this one should fail
    //         because PoX 1 is now defunct. Checked via the tx receipt.
    assert_eq!(alice_txs.len(), 3, "Alice should have 3 confirmed txs");
    // Bob should have two accepted transactions:
    //  TX0 -> Bob's initial lockup in PoX 2
    //  TX1 -> Bob's attempt to lock again in PoX 1 -- this one should fail
    //         because PoX 1 is now defunct. Checked via the tx receipt.
    assert_eq!(bob_txs.len(), 2, "Bob should have 2 confirmed txs");
    // Charlie should have one accepted transactions:
    //  TX0 -> Charlie's delegation in PoX 2. This tx just checks that the
    //         initialization code tracks costs in txs that occur after the
    //         initialization code (which uses a free tracker).
    assert_eq!(charlie_txs.len(), 1, "Charlie should have 1 confirmed txs");

    //  TX0 -> Alice's initial lockup in PoX 1
    assert!(
        match alice_txs.get(&0).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Alice tx0 should have committed okay"
    );

    //  TX1 -> Alice's PoX 2 lockup
    assert!(
        match alice_txs.get(&1).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Alice tx1 should have committed okay"
    );

    //  TX2 -> Alice's attempt to lock again in PoX 1 -- this one should fail
    //         because PoX 1 is now defunct. Checked via the tx receipt.
    assert_eq!(
        alice_txs.get(&2).unwrap().result,
        Value::err_none(),
        "Alice tx2 should have resulted in a runtime error"
    );

    //  TX0 -> Bob's initial lockup in PoX 2
    assert!(
        match bob_txs.get(&0).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Bob tx0 should have committed okay"
    );

    //  TX1 -> Bob's attempt to lock again in PoX 1 -- this one should fail
    //         because PoX 1 is now defunct. Checked via the tx receipt.
    assert_eq!(
        bob_txs.get(&1).unwrap().result,
        Value::err_none(),
        "Bob tx1 should have resulted in a runtime error"
    );

    //  TX0 -> Charlie's delegation in PoX 2. This tx just checks that the
    //         initialization code tracks costs in txs that occur after the
    //         initialization code (which uses a free tracker).
    assert!(
        match charlie_txs.get(&0).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Charlie tx0 should have committed okay"
    );
}

#[test]
fn test_simple_pox_2_auto_unlock_ab() {
    test_simple_pox_2_auto_unlock(true)
}

#[test]
fn test_simple_pox_2_auto_unlock_ba() {
    test_simple_pox_2_auto_unlock(false)
}

/// In this test case, two Stackers, Alice and Bob stack and interact with the
///  PoX v1 contract and PoX v2 contract across the epoch transition.
///
/// Alice: stacks via PoX v1 for 4 cycles. The third of these cycles occurs after
///        the PoX v1 -> v2 transition, and so Alice gets "early unlocked".
///        After the early unlock, Alice re-stacks in PoX v2
///        Alice tries to stack again via PoX v1, which is allowed by the contract,
///        but forbidden by the VM (because PoX has transitioned to v2)
/// Bob:   stacks via PoX v2 for 6 cycles. He attempted to stack via PoX v1 as well,
///        but is forbidden because he has already placed an account lock via PoX v2.
///
/// Note: this test is symmetric over the order of alice and bob's stacking calls.
///       when alice goes first, the auto-unlock code doesn't need to perform a "move"
///       when bob goes first, the auto-unlock code does need to perform a "move"
fn test_simple_pox_2_auto_unlock(alice_first: bool) {
    // this is the number of blocks after the first sortition any V1
    // PoX locks will automatically unlock at.
    let AUTO_UNLOCK_HEIGHT = 12;
    let EXPECTED_FIRST_V2_CYCLE = 8;
    // the sim environment produces 25 empty sortitions before
    //  tenures start being tracked.
    let EMPTY_SORTITIONS = 25;

    let mut burnchain = Burnchain::default_unittest(0, &BurnchainHeaderHash::zero());
    burnchain.pox_constants.reward_cycle_length = 5;
    burnchain.pox_constants.prepare_length = 2;
    burnchain.pox_constants.anchor_threshold = 1;
    burnchain.pox_constants.pox_participation_threshold_pct = 1;
    burnchain.pox_constants.v1_unlock_height = AUTO_UNLOCK_HEIGHT + EMPTY_SORTITIONS;

    let first_v2_cycle = burnchain
        .block_height_to_reward_cycle(burnchain.pox_constants.v1_unlock_height as u64)
        .unwrap()
        + 1;

    assert_eq!(first_v2_cycle, EXPECTED_FIRST_V2_CYCLE);

    eprintln!("First v2 cycle = {}", first_v2_cycle);

    let epochs = StacksEpoch::all(0, 0, EMPTY_SORTITIONS as u64 + 10);

    let observer = TestEventObserver::new();

    let (mut peer, mut keys) = instantiate_pox_peer_with_epoch(
        &burnchain,
        &format!("test_simple_pox_2_auto_unlock_{}", alice_first),
        6002,
        Some(epochs.clone()),
        Some(&observer),
    );

    peer.config.check_pox_invariants =
        Some((EXPECTED_FIRST_V2_CYCLE, EXPECTED_FIRST_V2_CYCLE + 10));

    let num_blocks = 35;

    let alice = keys.pop().unwrap();
    let bob = keys.pop().unwrap();
    let charlie = keys.pop().unwrap();

    let mut coinbase_nonce = 0;

    // produce blocks until the epoch switch
    for _i in 0..10 {
        peer.tenure_with_txs(&[], &mut coinbase_nonce);
    }

    // in the next tenure, PoX 2 should now exist.
    // Lets have Bob lock up for v2
    // this will lock for cycles 8, 9, 10, and 11
    //  the first v2 cycle will be 8
    let tip = get_tip(peer.sortdb.as_ref());

    let alice_lockup = make_pox_2_lockup(
        &alice,
        0,
        1024 * POX_THRESHOLD_STEPS_USTX,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        6,
        tip.block_height,
    );

    let bob_lockup = make_pox_2_lockup(
        &bob,
        0,
        1 * POX_THRESHOLD_STEPS_USTX,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&bob).bytes,
        6,
        tip.block_height,
    );

    // our "tenure counter" is now at 10
    assert_eq!(tip.block_height, 10 + EMPTY_SORTITIONS as u64);

    let txs = if alice_first {
        [alice_lockup, bob_lockup]
    } else {
        [bob_lockup, alice_lockup]
    };
    let mut latest_block = peer.tenure_with_txs(&txs, &mut coinbase_nonce);

    // check that the "raw" reward set will contain entries for alice and bob
    //  at the cycle start
    for cycle_number in EXPECTED_FIRST_V2_CYCLE..(EXPECTED_FIRST_V2_CYCLE + 6) {
        let cycle_start = burnchain.reward_cycle_to_block_height(cycle_number);
        let reward_set_entries = get_reward_set_entries_at(&mut peer, &latest_block, cycle_start);
        assert_eq!(reward_set_entries.len(), 2);
        assert_eq!(
            reward_set_entries[0].reward_address.bytes(),
            key_to_stacks_addr(&bob).bytes.0.to_vec()
        );
        assert_eq!(
            reward_set_entries[1].reward_address.bytes(),
            key_to_stacks_addr(&alice).bytes.0.to_vec()
        );
    }

    // we'll produce blocks until the next reward cycle gets through the "handled start" code
    //  this is one block after the reward cycle starts
    let height_target = burnchain.reward_cycle_to_block_height(EXPECTED_FIRST_V2_CYCLE) + 1;

    // but first, check that bob has locked tokens at (height_target + 1)
    let (bob_bal, _) = get_stx_account_at(
        &mut peer,
        &latest_block,
        &key_to_stacks_addr(&bob).to_account_principal(),
    )
    .canonical_repr_at_block(height_target + 1, burnchain.pox_constants.v1_unlock_height);
    assert_eq!(bob_bal.amount_locked(), POX_THRESHOLD_STEPS_USTX);

    while get_tip(peer.sortdb.as_ref()).block_height < height_target {
        latest_block = peer.tenure_with_txs(&[], &mut coinbase_nonce);
    }

    // check that the "raw" reward sets for all cycles just contains entries for alice
    //  at the cycle start
    for cycle_number in EXPECTED_FIRST_V2_CYCLE..(EXPECTED_FIRST_V2_CYCLE + 6) {
        let cycle_start = burnchain.reward_cycle_to_block_height(cycle_number);
        let reward_set_entries = get_reward_set_entries_at(&mut peer, &latest_block, cycle_start);
        assert_eq!(reward_set_entries.len(), 1);
        assert_eq!(
            reward_set_entries[0].reward_address.bytes(),
            key_to_stacks_addr(&alice).bytes.0.to_vec()
        );
    }

    // now check that bob has no locked tokens at (height_target + 1)
    let (bob_bal, _) = get_stx_account_at(
        &mut peer,
        &latest_block,
        &key_to_stacks_addr(&bob).to_account_principal(),
    )
    .canonical_repr_at_block(height_target + 1, burnchain.pox_constants.v1_unlock_height);
    assert_eq!(bob_bal.amount_locked(), 0);

    // but bob's still locked at (height_target): the unlock is accelerated to the "next" burn block
    let (bob_bal, _) = get_stx_account_at(
        &mut peer,
        &latest_block,
        &key_to_stacks_addr(&bob).to_account_principal(),
    )
    .canonical_repr_at_block(height_target + 1, burnchain.pox_constants.v1_unlock_height);
    assert_eq!(bob_bal.amount_locked(), 0);

    // check that the total reward cycle amounts have decremented correctly
    for cycle_number in EXPECTED_FIRST_V2_CYCLE..(EXPECTED_FIRST_V2_CYCLE + 6) {
        assert_eq!(
            get_reward_cycle_total(&mut peer, &latest_block, cycle_number),
            1024 * POX_THRESHOLD_STEPS_USTX
        );
    }

    // check that bob's stacking-state is gone and alice's stacking-state is correct
    assert!(
        get_stacking_state_pox_2(
            &mut peer,
            &latest_block,
            &key_to_stacks_addr(&bob).to_account_principal()
        )
        .is_none(),
        "Bob should not have a stacking-state entry"
    );

    let alice_state = get_stacking_state_pox_2(
        &mut peer,
        &latest_block,
        &key_to_stacks_addr(&alice).to_account_principal(),
    )
    .expect("Alice should have stacking-state entry")
    .expect_tuple();
    let reward_indexes_str = format!("{}", alice_state.get("reward-set-indexes").unwrap());
    assert_eq!(reward_indexes_str, "(u0 u0 u0 u0 u0 u0)");

    // now let's check some tx receipts

    let alice_address = key_to_stacks_addr(&alice);
    let bob_address = key_to_stacks_addr(&bob);
    let charlie_address = key_to_stacks_addr(&charlie);
    let blocks = observer.get_blocks();

    let mut alice_txs = HashMap::new();
    let mut bob_txs = HashMap::new();
    let mut charlie_txs = HashMap::new();

    eprintln!("Alice addr: {}", alice_address);
    eprintln!("Bob addr: {}", bob_address);

    for b in blocks.into_iter() {
        for r in b.receipts.into_iter() {
            if let TransactionOrigin::Stacks(ref t) = r.transaction {
                let addr = t.auth.origin().address_testnet();
                eprintln!("TX addr: {}", addr);
                if addr == alice_address {
                    alice_txs.insert(t.auth.get_origin_nonce(), r);
                } else if addr == bob_address {
                    bob_txs.insert(t.auth.get_origin_nonce(), r);
                } else if addr == charlie_address {
                    assert!(
                        r.execution_cost != ExecutionCost::zero(),
                        "Execution cost is not zero!"
                    );
                    charlie_txs.insert(t.auth.get_origin_nonce(), r);
                }
            }
        }
    }

    assert_eq!(alice_txs.len(), 1);
    assert_eq!(charlie_txs.len(), 0);

    assert_eq!(bob_txs.len(), 1);

    //  TX0 -> Bob's initial lockup in PoX 2
    assert!(
        match bob_txs.get(&0).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Bob tx0 should have committed okay"
    );
}

/// In this test case, Alice delegates to Bob.
///  Bob stacks Alice's funds via PoX v2 for 6 cycles. In the third cycle,
///  Bob increases Alice's stacking amount.
///
#[test]
fn delegate_stack_increase() {
    // this is the number of blocks after the first sortition any V1
    // PoX locks will automatically unlock at.
    let AUTO_UNLOCK_HEIGHT = 12;
    let EXPECTED_FIRST_V2_CYCLE = 8;
    // the sim environment produces 25 empty sortitions before
    //  tenures start being tracked.
    let EMPTY_SORTITIONS = 25;

    let mut burnchain = Burnchain::default_unittest(0, &BurnchainHeaderHash::zero());
    burnchain.pox_constants.reward_cycle_length = 5;
    burnchain.pox_constants.prepare_length = 2;
    burnchain.pox_constants.anchor_threshold = 1;
    burnchain.pox_constants.pox_participation_threshold_pct = 1;
    burnchain.pox_constants.v1_unlock_height = AUTO_UNLOCK_HEIGHT + EMPTY_SORTITIONS;

    let first_v2_cycle = burnchain
        .block_height_to_reward_cycle(burnchain.pox_constants.v1_unlock_height as u64)
        .unwrap()
        + 1;

    assert_eq!(first_v2_cycle, EXPECTED_FIRST_V2_CYCLE);

    eprintln!("First v2 cycle = {}", first_v2_cycle);

    let epochs = StacksEpoch::all(0, 0, EMPTY_SORTITIONS as u64 + 10);

    let observer = TestEventObserver::new();

    let (mut peer, mut keys) = instantiate_pox_peer_with_epoch(
        &burnchain,
        &format!("pox_2_delegate_stack_increase"),
        6002,
        Some(epochs.clone()),
        Some(&observer),
    );

    peer.config.check_pox_invariants =
        Some((EXPECTED_FIRST_V2_CYCLE, EXPECTED_FIRST_V2_CYCLE + 10));

    let num_blocks = 35;

    let alice = keys.pop().unwrap();
    let alice_address = key_to_stacks_addr(&alice);
    let alice_principal = PrincipalData::from(alice_address.clone());
    let bob = keys.pop().unwrap();
    let bob_address = key_to_stacks_addr(&bob);
    let bob_principal = PrincipalData::from(bob_address.clone());
    let bob_pox_addr = make_pox_addr(AddressHashMode::SerializeP2PKH, bob_address.bytes.clone());
    let mut alice_nonce = 0;
    let mut bob_nonce = 0;

    let alice_delegation_amount = 1023 * POX_THRESHOLD_STEPS_USTX;
    let alice_first_lock_amount = 512 * POX_THRESHOLD_STEPS_USTX;

    let mut coinbase_nonce = 0;

    // produce blocks until the epoch switch
    for _i in 0..10 {
        peer.tenure_with_txs(&[], &mut coinbase_nonce);
    }

    // in the next tenure, PoX 2 should now exist.
    let tip = get_tip(peer.sortdb.as_ref());

    // submit delegation tx
    let alice_delegation_1 = make_pox_2_contract_call(
        &alice,
        alice_nonce,
        "delegate-stx",
        vec![
            Value::UInt(alice_delegation_amount),
            bob_principal.clone().into(),
            Value::none(),
            Value::none(),
        ],
    );

    alice_nonce += 1;

    let delegate_stack_tx = make_pox_2_contract_call(
        &bob,
        bob_nonce,
        "delegate-stack-stx",
        vec![
            alice_principal.clone().into(),
            Value::UInt(alice_first_lock_amount),
            bob_pox_addr.clone(),
            Value::UInt(tip.block_height as u128),
            Value::UInt(6),
        ],
    );

    bob_nonce += 1;

    // our "tenure counter" is now at 10
    assert_eq!(tip.block_height, 10 + EMPTY_SORTITIONS as u64);

    let mut latest_block = peer.tenure_with_txs(
        &[alice_delegation_1, delegate_stack_tx],
        &mut coinbase_nonce,
    );

    // check that the partial stacking state contains entries for bob
    for cycle_number in EXPECTED_FIRST_V2_CYCLE..(EXPECTED_FIRST_V2_CYCLE + 6) {
        let partial_stacked = get_partial_stacked(
            &mut peer,
            &latest_block,
            &bob_pox_addr,
            cycle_number,
            &bob_principal,
        );
        assert_eq!(partial_stacked, 512 * POX_THRESHOLD_STEPS_USTX);
    }

    // we'll produce blocks until the 3rd reward cycle gets through the "handled start" code
    //  this is one block after the reward cycle starts
    let height_target = burnchain.reward_cycle_to_block_height(EXPECTED_FIRST_V2_CYCLE + 3) + 1;

    while get_tip(peer.sortdb.as_ref()).block_height < height_target {
        latest_block = peer.tenure_with_txs(&[], &mut coinbase_nonce);
    }

    let alice_bal = get_stx_account_at(&mut peer, &latest_block, &alice_principal);

    assert_eq!(alice_bal.amount_locked(), alice_first_lock_amount);

    // check that the partial stacking state contains entries for bob
    for cycle_number in EXPECTED_FIRST_V2_CYCLE..(EXPECTED_FIRST_V2_CYCLE + 6) {
        let partial_stacked = get_partial_stacked(
            &mut peer,
            &latest_block,
            &bob_pox_addr,
            cycle_number,
            &bob_principal,
        );
        assert_eq!(partial_stacked, 512 * POX_THRESHOLD_STEPS_USTX);
    }

    let mut txs_to_submit = vec![];

    let fail_direct_increase_delegation = alice_nonce;
    txs_to_submit.push(make_pox_2_contract_call(
        &alice,
        alice_nonce,
        "stack-increase",
        vec![Value::UInt(1)],
    ));
    alice_nonce += 1;

    let fail_delegate_too_much_locked = bob_nonce;
    txs_to_submit.push(make_pox_2_contract_call(
        &bob,
        bob_nonce,
        "delegate-stack-increase",
        vec![
            alice_principal.clone().into(),
            bob_pox_addr.clone(),
            Value::UInt(alice_delegation_amount - alice_first_lock_amount + 1),
        ],
    ));
    bob_nonce += 1;

    let fail_invalid_amount = bob_nonce;
    txs_to_submit.push(make_pox_2_contract_call(
        &bob,
        bob_nonce,
        "delegate-stack-increase",
        vec![
            alice_principal.clone().into(),
            bob_pox_addr.clone(),
            Value::UInt(0),
        ],
    ));
    bob_nonce += 1;

    let fail_insufficient_funds = bob_nonce;
    txs_to_submit.push(make_pox_2_contract_call(
        &bob,
        bob_nonce,
        "delegate-stack-increase",
        vec![
            alice_principal.clone().into(),
            bob_pox_addr.clone(),
            Value::UInt(alice_bal.amount_unlocked() + 1),
        ],
    ));
    bob_nonce += 1;

    txs_to_submit.push(make_pox_2_contract_call(
        &bob,
        bob_nonce,
        "delegate-stack-increase",
        vec![
            alice_principal.clone().into(),
            bob_pox_addr.clone(),
            Value::UInt(alice_delegation_amount - alice_first_lock_amount),
        ],
    ));
    bob_nonce += 1;

    latest_block = peer.tenure_with_txs(&txs_to_submit, &mut coinbase_nonce);

    assert_eq!(
        get_stx_account_at(&mut peer, &latest_block, &alice_principal).amount_locked(),
        alice_delegation_amount
    );

    // check that the partial stacking state contains entries for bob and they've incremented correctly
    for cycle_number in (EXPECTED_FIRST_V2_CYCLE)..(EXPECTED_FIRST_V2_CYCLE + 4) {
        let partial_stacked = get_partial_stacked(
            &mut peer,
            &latest_block,
            &bob_pox_addr,
            cycle_number,
            &bob_principal,
        );
        assert_eq!(partial_stacked, alice_first_lock_amount);
    }

    for cycle_number in (EXPECTED_FIRST_V2_CYCLE + 4)..(EXPECTED_FIRST_V2_CYCLE + 6) {
        let partial_stacked = get_partial_stacked(
            &mut peer,
            &latest_block,
            &bob_pox_addr,
            cycle_number,
            &bob_principal,
        );
        assert_eq!(partial_stacked, alice_delegation_amount,);
    }

    // now let's check some tx receipts

    let alice_address = key_to_stacks_addr(&alice);
    let blocks = observer.get_blocks();

    let mut alice_txs = HashMap::new();
    let mut bob_txs = HashMap::new();

    for b in blocks.into_iter() {
        for r in b.receipts.into_iter() {
            if let TransactionOrigin::Stacks(ref t) = r.transaction {
                let addr = t.auth.origin().address_testnet();
                if addr == alice_address {
                    alice_txs.insert(t.auth.get_origin_nonce(), r);
                } else if addr == bob_address {
                    bob_txs.insert(t.auth.get_origin_nonce(), r);
                }
            }
        }
    }

    assert_eq!(alice_txs.len() as u64, 2);
    assert_eq!(bob_txs.len() as u64, 5);

    // transaction should fail because Alice cannot increase her own stacking amount while delegating
    assert_eq!(
        &alice_txs[&fail_direct_increase_delegation]
            .result
            .to_string(),
        "(err 20)"
    );

    // transaction should fail because Alice did not delegate enough funds to Bob
    assert_eq!(
        &bob_txs[&fail_delegate_too_much_locked].result.to_string(),
        "(err 22)"
    );

    // transaction should fail because Alice doesn't have enough funds
    assert_eq!(
        &bob_txs[&fail_insufficient_funds].result.to_string(),
        "(err 1)"
    );

    // transaction should fail because the amount supplied is invalid (i.e., 0)
    assert_eq!(
        &bob_txs[&fail_invalid_amount].result.to_string(),
        "(err 18)"
    );
}

/// In this test case, Alice stacks and interacts with the
///  PoX v2 contract after the epoch transition.
///
/// Alice: stacks via PoX v2 for 6 cycles. In the third cycle, Alice invokes
/// `stack-increase`
///
#[test]
fn stack_increase() {
    // this is the number of blocks after the first sortition any V1
    // PoX locks will automatically unlock at.
    let AUTO_UNLOCK_HEIGHT = 12;
    let EXPECTED_FIRST_V2_CYCLE = 8;
    // the sim environment produces 25 empty sortitions before
    //  tenures start being tracked.
    let EMPTY_SORTITIONS = 25;

    let mut burnchain = Burnchain::default_unittest(0, &BurnchainHeaderHash::zero());
    burnchain.pox_constants.reward_cycle_length = 5;
    burnchain.pox_constants.prepare_length = 2;
    burnchain.pox_constants.anchor_threshold = 1;
    burnchain.pox_constants.pox_participation_threshold_pct = 1;
    burnchain.pox_constants.v1_unlock_height = AUTO_UNLOCK_HEIGHT + EMPTY_SORTITIONS;

    let first_v2_cycle = burnchain
        .block_height_to_reward_cycle(burnchain.pox_constants.v1_unlock_height as u64)
        .unwrap()
        + 1;

    assert_eq!(first_v2_cycle, EXPECTED_FIRST_V2_CYCLE);

    eprintln!("First v2 cycle = {}", first_v2_cycle);

    let epochs = StacksEpoch::all(0, 0, EMPTY_SORTITIONS as u64 + 10);

    let observer = TestEventObserver::new();

    let (mut peer, mut keys) = instantiate_pox_peer_with_epoch(
        &burnchain,
        &format!("test_simple_pox_2_increase"),
        6002,
        Some(epochs.clone()),
        Some(&observer),
    );

    peer.config.check_pox_invariants =
        Some((EXPECTED_FIRST_V2_CYCLE, EXPECTED_FIRST_V2_CYCLE + 10));

    let num_blocks = 35;

    let alice = keys.pop().unwrap();
    let alice_address = key_to_stacks_addr(&alice);
    let alice_principal = PrincipalData::from(alice_address.clone());
    let mut alice_nonce = 0;

    let mut coinbase_nonce = 0;

    let first_lockup_amt = 512 * POX_THRESHOLD_STEPS_USTX;
    let total_balance = 1024 * POX_THRESHOLD_STEPS_USTX;
    let increase_amt = total_balance - first_lockup_amt;

    // produce blocks until the epoch switch
    for _i in 0..10 {
        peer.tenure_with_txs(&[], &mut coinbase_nonce);
    }

    // in the next tenure, PoX 2 should now exist.
    let tip = get_tip(peer.sortdb.as_ref());

    // submit an increase: this should fail, because Alice is not yet locked
    let fail_no_lock_tx = alice_nonce;
    let alice_increase = make_pox_2_increase(&alice, alice_nonce, increase_amt);
    alice_nonce += 1;

    let alice_lockup = make_pox_2_lockup(
        &alice,
        alice_nonce,
        first_lockup_amt,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        6,
        tip.block_height,
    );
    alice_nonce += 1;

    // our "tenure counter" is now at 10
    assert_eq!(tip.block_height, 10 + EMPTY_SORTITIONS as u64);

    let mut latest_block =
        peer.tenure_with_txs(&[alice_increase, alice_lockup], &mut coinbase_nonce);

    assert_eq!(
        get_stx_account_at(&mut peer, &latest_block, &alice_principal).amount_locked(),
        first_lockup_amt,
    );

    assert_eq!(
        get_stx_account_at(&mut peer, &latest_block, &alice_principal).get_total_balance(),
        total_balance,
    );

    // check that the "raw" reward set will contain entries for alice at the cycle start
    for cycle_number in EXPECTED_FIRST_V2_CYCLE..(EXPECTED_FIRST_V2_CYCLE + 6) {
        let cycle_start = burnchain.reward_cycle_to_block_height(cycle_number);
        let reward_set_entries = get_reward_set_entries_at(&mut peer, &latest_block, cycle_start);
        assert_eq!(reward_set_entries.len(), 1);
        assert_eq!(
            reward_set_entries[0].reward_address.bytes(),
            key_to_stacks_addr(&alice).bytes.0.to_vec()
        );
        assert_eq!(reward_set_entries[0].amount_stacked, first_lockup_amt,);
    }

    // we'll produce blocks until the 3rd reward cycle gets through the "handled start" code
    //  this is one block after the reward cycle starts
    let height_target = burnchain.reward_cycle_to_block_height(EXPECTED_FIRST_V2_CYCLE + 3) + 1;

    while get_tip(peer.sortdb.as_ref()).block_height < height_target {
        latest_block = peer.tenure_with_txs(&[], &mut coinbase_nonce);
    }

    // check that the "raw" reward sets for all cycles contains entries for alice
    for cycle_number in EXPECTED_FIRST_V2_CYCLE..(EXPECTED_FIRST_V2_CYCLE + 6) {
        let cycle_start = burnchain.reward_cycle_to_block_height(cycle_number);
        let reward_set_entries = get_reward_set_entries_at(&mut peer, &latest_block, cycle_start);
        assert_eq!(reward_set_entries.len(), 1);
        assert_eq!(
            reward_set_entries[0].reward_address.bytes(),
            key_to_stacks_addr(&alice).bytes.0.to_vec()
        );
        assert_eq!(reward_set_entries[0].amount_stacked, first_lockup_amt,);
    }

    let mut txs_to_submit = vec![];
    let fail_bad_amount = alice_nonce;
    txs_to_submit.push(make_pox_2_increase(&alice, alice_nonce, 0));
    alice_nonce += 1;

    // this stack-increase tx should work
    txs_to_submit.push(make_pox_2_increase(&alice, alice_nonce, increase_amt));
    alice_nonce += 1;

    // increase by an amount we don't have!
    let fail_not_enough_funds = alice_nonce;
    txs_to_submit.push(make_pox_2_increase(&alice, alice_nonce, 1));
    alice_nonce += 1;

    latest_block = peer.tenure_with_txs(&txs_to_submit, &mut coinbase_nonce);

    assert_eq!(
        get_stx_account_at(&mut peer, &latest_block, &alice_principal).amount_locked(),
        first_lockup_amt + increase_amt,
    );

    assert_eq!(
        get_stx_account_at(&mut peer, &latest_block, &alice_principal).get_total_balance(),
        total_balance,
    );

    // check that the total reward cycle amounts have incremented correctly
    for cycle_number in (EXPECTED_FIRST_V2_CYCLE)..(EXPECTED_FIRST_V2_CYCLE + 4) {
        assert_eq!(
            get_reward_cycle_total(&mut peer, &latest_block, cycle_number),
            first_lockup_amt,
        );
        let cycle_start = burnchain.reward_cycle_to_block_height(cycle_number);
        let reward_set_entries = get_reward_set_entries_at(&mut peer, &latest_block, cycle_start);
        assert_eq!(reward_set_entries.len(), 1);
        assert_eq!(
            reward_set_entries[0].reward_address.bytes(),
            key_to_stacks_addr(&alice).bytes.0.to_vec()
        );
        assert_eq!(reward_set_entries[0].amount_stacked, first_lockup_amt,);
    }

    for cycle_number in (EXPECTED_FIRST_V2_CYCLE + 4)..(EXPECTED_FIRST_V2_CYCLE + 6) {
        assert_eq!(
            get_reward_cycle_total(&mut peer, &latest_block, cycle_number),
            first_lockup_amt + increase_amt,
        );
        let cycle_start = burnchain.reward_cycle_to_block_height(cycle_number);
        let reward_set_entries = get_reward_set_entries_at(&mut peer, &latest_block, cycle_start);
        assert_eq!(reward_set_entries.len(), 1);
        assert_eq!(
            reward_set_entries[0].reward_address.bytes(),
            key_to_stacks_addr(&alice).bytes.0.to_vec()
        );
        assert_eq!(
            reward_set_entries[0].amount_stacked,
            first_lockup_amt + increase_amt,
        );
    }

    // now let's check some tx receipts
    let blocks = observer.get_blocks();

    let mut alice_txs = HashMap::new();

    for b in blocks.into_iter() {
        for r in b.receipts.into_iter() {
            if let TransactionOrigin::Stacks(ref t) = r.transaction {
                let addr = t.auth.origin().address_testnet();
                if addr == alice_address {
                    alice_txs.insert(t.auth.get_origin_nonce(), r);
                }
            }
        }
    }

    assert_eq!(alice_txs.len() as u64, alice_nonce);

    // transaction should fail because lock isn't applied
    assert_eq!(&alice_txs[&fail_no_lock_tx].result.to_string(), "(err 26)");

    // transaction should fail because Alice doesn't have enough funds
    assert_eq!(
        &alice_txs[&fail_not_enough_funds].result.to_string(),
        "(err 1)"
    );

    // transaction should fail because the amount supplied is invalid (i.e., 0)
    assert_eq!(&alice_txs[&fail_bad_amount].result.to_string(), "(err 18)");
}

#[test]
fn test_lock_period_invariant_extend_transition() {
    // this is the number of blocks after the first sortition any V1
    // PoX locks will automatically unlock at.
    let AUTO_UNLOCK_HT = 25;
    let EXPECTED_FIRST_V2_CYCLE = 11;
    // the sim environment produces 25 empty sortitions before
    //  tenures start being tracked.
    let EMPTY_SORTITIONS = 25;

    let mut burnchain = Burnchain::default_unittest(0, &BurnchainHeaderHash::zero());
    burnchain.pox_constants.reward_cycle_length = 5;
    burnchain.pox_constants.prepare_length = 2;
    burnchain.pox_constants.anchor_threshold = 1;
    burnchain.pox_constants.v1_unlock_height = AUTO_UNLOCK_HT + EMPTY_SORTITIONS;

    let first_v2_cycle = burnchain
        .block_height_to_reward_cycle(burnchain.pox_constants.v1_unlock_height as u64)
        .unwrap()
        + 1;

    eprintln!("First v2 cycle = {}", first_v2_cycle);
    assert_eq!(first_v2_cycle, EXPECTED_FIRST_V2_CYCLE);

    let epochs = StacksEpoch::all(0, 0, EMPTY_SORTITIONS as u64 + 10);

    let observer = TestEventObserver::new();

    let (mut peer, mut keys) = instantiate_pox_peer_with_epoch(
        &burnchain,
        "test_pox_extend_transition_pox_2",
        6002,
        Some(epochs.clone()),
        Some(&observer),
    );

    peer.config.check_pox_invariants =
        Some((EXPECTED_FIRST_V2_CYCLE, EXPECTED_FIRST_V2_CYCLE + 10));

    let num_blocks = 35;

    let alice = keys.pop().unwrap();

    let EXPECTED_ALICE_FIRST_REWARD_CYCLE = 6;
    let mut coinbase_nonce = 0;

    let INITIAL_BALANCE = 1024 * POX_THRESHOLD_STEPS_USTX;
    let ALICE_LOCKUP = 1024 * POX_THRESHOLD_STEPS_USTX;

    // our "tenure counter" is now at 0
    let tip = get_tip(peer.sortdb.as_ref());
    assert_eq!(tip.block_height, 0 + EMPTY_SORTITIONS as u64);

    // first tenure is empty
    peer.tenure_with_txs(&[], &mut coinbase_nonce);

    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, INITIAL_BALANCE);

    let alice_account = get_account(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_account.stx_balance.amount_unlocked(), INITIAL_BALANCE);
    assert_eq!(alice_account.stx_balance.amount_locked(), 0);
    assert_eq!(alice_account.stx_balance.unlock_height(), 0);

    // next tenure include Alice's lockup
    let tip = get_tip(peer.sortdb.as_ref());
    let alice_lockup = make_pox_lockup(
        &alice,
        0,
        ALICE_LOCKUP,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        4,
        tip.block_height,
    );

    // our "tenure counter" is now at 1
    assert_eq!(tip.block_height, 1 + EMPTY_SORTITIONS as u64);

    let tip_index_block = peer.tenure_with_txs(&[alice_lockup], &mut coinbase_nonce);

    // check the stacking minimum
    let total_liquid_ustx = get_liquid_ustx(&mut peer);
    let min_ustx = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
        chainstate.get_stacking_minimum(sortdb, &tip_index_block)
    })
    .unwrap();
    assert_eq!(
        min_ustx,
        total_liquid_ustx / POX_TESTNET_STACKING_THRESHOLD_25
    );

    // no reward addresses
    let reward_addrs = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
        get_reward_addresses_with_par_tip(chainstate, &burnchain, sortdb, &tip_index_block)
    })
    .unwrap();
    assert_eq!(reward_addrs.len(), 0);

    // check the first reward cycle when Alice's tokens get stacked
    let tip_burn_block_height = get_par_burn_block_height(peer.chainstate(), &tip_index_block);
    let alice_first_reward_cycle = 1 + burnchain
        .block_height_to_reward_cycle(tip_burn_block_height)
        .unwrap() as u128;

    assert_eq!(alice_first_reward_cycle, EXPECTED_ALICE_FIRST_REWARD_CYCLE);

    // alice locked, so balance should be 0
    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, 0);

    // produce blocks until alice's first reward cycle
    for _i in 0..4 {
        peer.tenure_with_txs(&[], &mut coinbase_nonce);
    }

    // produce blocks until immediately after the epoch switch (4 more blocks to block height 36)
    for _i in 0..4 {
        let tip_index_block = peer.tenure_with_txs(&[], &mut coinbase_nonce);

        // alice is still locked, balance should be 0
        let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
        assert_eq!(alice_balance, 0);
    }

    // in the next tenure, PoX 2 should now exist.
    // Lets have Bob lock up for v2
    // this will lock for cycles 8, 9, 10
    //  the first v2 cycle will be 8
    let tip = get_tip(peer.sortdb.as_ref());

    // Alice _will_ auto-unlock: she can stack-extend in PoX v2
    let alice_lockup = make_pox_2_extend(
        &alice,
        1,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        6,
    );

    // our "tenure counter" is now at 10
    assert_eq!(tip.block_height, 10 + EMPTY_SORTITIONS as u64);

    let tip_index_block = peer.tenure_with_txs(&[alice_lockup], &mut coinbase_nonce);
}

/// In this test case, two Stackers, Alice and Bob stack and interact with the
///  PoX v1 contract and PoX v2 contract across the epoch transition. This test
///  covers the two different ways a Stacker can validly extend via `stack-extend` --
///  extending from a V1 lockup and extending from a V2 lockup.
///
/// Alice: stacks via PoX v1 for 4 cycles. The third of these cycles occurs after
///        the PoX v1 -> v2 transition, and so Alice gets "early unlocked".
///        Before the early unlock, Alice invokes `stack-extend` in PoX v2
///        Alice tries to stack again via PoX v1, which is allowed by the contract,
///        but forbidden by the VM (because PoX has transitioned to v2)
/// Bob:   stacks via PoX v2 for 3 cycles.
///        Bob extends 1 cycles
#[test]
fn test_pox_extend_transition_pox_2() {
    // this is the number of blocks after the first sortition any V1
    // PoX locks will automatically unlock at.
    let AUTO_UNLOCK_HT = 12;
    let EXPECTED_FIRST_V2_CYCLE = 8;
    // the sim environment produces 25 empty sortitions before
    //  tenures start being tracked.
    let EMPTY_SORTITIONS = 25;

    let mut burnchain = Burnchain::default_unittest(
        0,
        &BurnchainHeaderHash::from_hex(BITCOIN_REGTEST_FIRST_BLOCK_HASH).unwrap(),
    );
    burnchain.pox_constants.reward_cycle_length = 5;
    burnchain.pox_constants.prepare_length = 2;
    burnchain.pox_constants.anchor_threshold = 1;
    burnchain.pox_constants.v1_unlock_height = AUTO_UNLOCK_HT + EMPTY_SORTITIONS;

    let first_v2_cycle = burnchain
        .block_height_to_reward_cycle(burnchain.pox_constants.v1_unlock_height as u64)
        .unwrap()
        + 1;

    eprintln!("First v2 cycle = {}", first_v2_cycle);
    assert_eq!(first_v2_cycle, EXPECTED_FIRST_V2_CYCLE);

    let epochs = StacksEpoch::all(0, 0, EMPTY_SORTITIONS as u64 + 10);

    let observer = TestEventObserver::new();

    let (mut peer, mut keys) = instantiate_pox_peer_with_epoch(
        &burnchain,
        "test_pox_extend_transition_pox_2",
        6002,
        Some(epochs.clone()),
        Some(&observer),
    );

    peer.config.check_pox_invariants =
        Some((EXPECTED_FIRST_V2_CYCLE, EXPECTED_FIRST_V2_CYCLE + 10));

    let num_blocks = 35;

    let alice = keys.pop().unwrap();
    let bob = keys.pop().unwrap();

    let EXPECTED_ALICE_FIRST_REWARD_CYCLE = 6;
    let mut coinbase_nonce = 0;

    let INITIAL_BALANCE = 1024 * POX_THRESHOLD_STEPS_USTX;
    let ALICE_LOCKUP = 1024 * POX_THRESHOLD_STEPS_USTX;
    let BOB_LOCKUP = 512 * POX_THRESHOLD_STEPS_USTX;

    // these checks should pass between Alice's first reward cycle,
    //  and the start of V2 reward cycles
    let alice_rewards_to_v2_start_checks = |tip_index_block, peer: &mut TestPeer| {
        let tip_burn_block_height = get_par_burn_block_height(peer.chainstate(), &tip_index_block);
        let cur_reward_cycle = burnchain
            .block_height_to_reward_cycle(tip_burn_block_height)
            .unwrap() as u128;
        let (min_ustx, reward_addrs, total_stacked) = with_sortdb(peer, |ref mut c, ref sortdb| {
            (
                c.get_stacking_minimum(sortdb, &tip_index_block).unwrap(),
                get_reward_addresses_with_par_tip(c, &burnchain, sortdb, &tip_index_block).unwrap(),
                c.test_get_total_ustx_stacked(sortdb, &tip_index_block, cur_reward_cycle)
                    .unwrap(),
            )
        });

        assert!(
            cur_reward_cycle >= EXPECTED_ALICE_FIRST_REWARD_CYCLE
                && cur_reward_cycle < first_v2_cycle as u128
        );
        //  Alice is the only Stacker, so check that.
        let (amount_ustx, pox_addr, lock_period, first_reward_cycle) =
            get_stacker_info(peer, &key_to_stacks_addr(&alice).into()).unwrap();
        eprintln!(
            "\nAlice: {} uSTX stacked for {} cycle(s); addr is {:?}; first reward cycle is {}\n",
            amount_ustx, lock_period, &pox_addr, first_reward_cycle
        );

        // one reward address, and it's Alice's
        // either way, there's a single reward address
        assert_eq!(reward_addrs.len(), 1);
        assert_eq!(
            (reward_addrs[0].0).version(),
            AddressHashMode::SerializeP2PKH as u8
        );
        assert_eq!(
            (reward_addrs[0].0).hash160(),
            key_to_stacks_addr(&alice).bytes
        );
        assert_eq!(reward_addrs[0].1, ALICE_LOCKUP);
    };

    // these checks should pass after the start of V2 reward cycles
    let v2_rewards_checks = |tip_index_block, peer: &mut TestPeer| {
        let tip_burn_block_height = get_par_burn_block_height(peer.chainstate(), &tip_index_block);
        let cur_reward_cycle = burnchain
            .block_height_to_reward_cycle(tip_burn_block_height)
            .unwrap() as u128;
        let (min_ustx, reward_addrs, total_stacked) = with_sortdb(peer, |ref mut c, ref sortdb| {
            (
                c.get_stacking_minimum(sortdb, &tip_index_block).unwrap(),
                get_reward_addresses_with_par_tip(c, &burnchain, sortdb, &tip_index_block).unwrap(),
                c.test_get_total_ustx_stacked(sortdb, &tip_index_block, cur_reward_cycle)
                    .unwrap(),
            )
        });

        eprintln!(
            "reward_cycle = {}, reward_addrs = {}, total_stacked = {}",
            cur_reward_cycle,
            reward_addrs.len(),
            total_stacked
        );

        assert!(cur_reward_cycle >= first_v2_cycle as u128);
        // v2 reward cycles have begun, so reward addrs should be read from PoX2 which is Bob + Alice
        assert_eq!(reward_addrs.len(), 2);
        assert_eq!(
            (reward_addrs[0].0).version(),
            AddressHashMode::SerializeP2PKH as u8
        );
        assert_eq!(
            (reward_addrs[0].0).hash160(),
            key_to_stacks_addr(&bob).bytes
        );
        assert_eq!(reward_addrs[0].1, BOB_LOCKUP);

        assert_eq!(
            (reward_addrs[1].0).version(),
            AddressHashMode::SerializeP2PKH as u8
        );
        assert_eq!(
            (reward_addrs[1].0).hash160(),
            key_to_stacks_addr(&alice).bytes
        );
        assert_eq!(reward_addrs[1].1, ALICE_LOCKUP);
    };

    // our "tenure counter" is now at 0
    let tip = get_tip(peer.sortdb.as_ref());
    assert_eq!(tip.block_height, 0 + EMPTY_SORTITIONS as u64);

    // first tenure is empty
    peer.tenure_with_txs(&[], &mut coinbase_nonce);

    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, INITIAL_BALANCE);

    let alice_account = get_account(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_account.stx_balance.amount_unlocked(), INITIAL_BALANCE);
    assert_eq!(alice_account.stx_balance.amount_locked(), 0);
    assert_eq!(alice_account.stx_balance.unlock_height(), 0);

    // next tenure include Alice's lockup
    let tip = get_tip(peer.sortdb.as_ref());
    let alice_lockup = make_pox_lockup(
        &alice,
        0,
        ALICE_LOCKUP,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        4,
        tip.block_height,
    );

    // our "tenure counter" is now at 1
    assert_eq!(tip.block_height, 1 + EMPTY_SORTITIONS as u64);

    let tip_index_block = peer.tenure_with_txs(&[alice_lockup], &mut coinbase_nonce);

    // check the stacking minimum
    let total_liquid_ustx = get_liquid_ustx(&mut peer);
    let min_ustx = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
        chainstate.get_stacking_minimum(sortdb, &tip_index_block)
    })
    .unwrap();
    assert_eq!(
        min_ustx,
        total_liquid_ustx / POX_TESTNET_STACKING_THRESHOLD_25
    );

    // no reward addresses
    let reward_addrs = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
        get_reward_addresses_with_par_tip(chainstate, &burnchain, sortdb, &tip_index_block)
    })
    .unwrap();
    assert_eq!(reward_addrs.len(), 0);

    // check the first reward cycle when Alice's tokens get stacked
    let tip_burn_block_height = get_par_burn_block_height(peer.chainstate(), &tip_index_block);
    let alice_first_reward_cycle = 1 + burnchain
        .block_height_to_reward_cycle(tip_burn_block_height)
        .unwrap() as u128;

    assert_eq!(alice_first_reward_cycle, EXPECTED_ALICE_FIRST_REWARD_CYCLE);

    // alice locked, so balance should be 0
    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, 0);

    // produce blocks until alice's first reward cycle
    for _i in 0..4 {
        peer.tenure_with_txs(&[], &mut coinbase_nonce);
    }

    // produce blocks until immediately after the epoch switch (8 more blocks to block height 36)
    for _i in 0..4 {
        let tip_index_block = peer.tenure_with_txs(&[], &mut coinbase_nonce);

        // alice is still locked, balance should be 0
        let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
        assert_eq!(alice_balance, 0);

        alice_rewards_to_v2_start_checks(tip_index_block, &mut peer);
    }

    // in the next tenure, PoX 2 should now exist.
    // Lets have Bob lock up for v2
    // this will lock for cycles 8, 9, 10
    //  the first v2 cycle will be 8
    let tip = get_tip(peer.sortdb.as_ref());

    let bob_lockup = make_pox_2_lockup(
        &bob,
        0,
        BOB_LOCKUP,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&bob).bytes,
        3,
        tip.block_height,
    );

    // Alice _will_ auto-unlock: she can stack-extend in PoX v2
    let alice_lockup = make_pox_2_extend(
        &alice,
        1,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        6,
    );

    // our "tenure counter" is now at 10
    assert_eq!(tip.block_height, 10 + EMPTY_SORTITIONS as u64);

    let tip_index_block = peer.tenure_with_txs(&[bob_lockup, alice_lockup], &mut coinbase_nonce);

    alice_rewards_to_v2_start_checks(tip_index_block, &mut peer);

    // Extend bob's lockup via `stack-extend` for 1 more cycle
    let bob_extend = make_pox_2_extend(
        &bob,
        1,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&bob).bytes,
        1,
    );

    let tip_index_block = peer.tenure_with_txs(&[bob_extend], &mut coinbase_nonce);

    alice_rewards_to_v2_start_checks(tip_index_block, &mut peer);

    // produce blocks until "tenure counter" is 15 -- this is where
    //  the v2 reward cycles start
    for _i in 0..3 {
        let tip_index_block = peer.tenure_with_txs(&[], &mut coinbase_nonce);

        // alice is still locked, balance should be 0
        let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
        assert_eq!(alice_balance, 0);

        alice_rewards_to_v2_start_checks(tip_index_block, &mut peer);
    }

    let tip = get_tip(peer.sortdb.as_ref());
    // our "tenure counter" is now at 15
    assert_eq!(tip.block_height, 15 + EMPTY_SORTITIONS as u64);

    // produce blocks until "tenure counter" is 32 -- this is where
    //  alice *would have been* unlocked under v1 rules
    for _i in 0..17 {
        let tip_index_block = peer.tenure_with_txs(&[], &mut coinbase_nonce);

        // alice is still locked, balance should be 0
        let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
        assert_eq!(alice_balance, 0);

        v2_rewards_checks(tip_index_block, &mut peer);
    }

    // our "tenure counter" is now at 32
    let tip = get_tip(peer.sortdb.as_ref());
    assert_eq!(tip.block_height, 32 + EMPTY_SORTITIONS as u64);

    // Alice would have unlocked under v1 rules, so try to stack again via PoX 1 and expect a runtime error
    // in the tx
    let alice_lockup = make_pox_lockup(
        &alice,
        2,
        512 * POX_THRESHOLD_STEPS_USTX,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        12,
        tip.block_height,
    );

    let tip_index_block = peer.tenure_with_txs(&[alice_lockup], &mut coinbase_nonce);

    v2_rewards_checks(tip_index_block, &mut peer);

    // now let's check some tx receipts

    let alice_address = key_to_stacks_addr(&alice);
    let bob_address = key_to_stacks_addr(&bob);
    let blocks = observer.get_blocks();

    let mut alice_txs = HashMap::new();
    let mut bob_txs = HashMap::new();

    eprintln!("Alice addr: {}", alice_address);
    eprintln!("Bob addr: {}", bob_address);

    for b in blocks.into_iter() {
        for r in b.receipts.into_iter() {
            if let TransactionOrigin::Stacks(ref t) = r.transaction {
                let addr = t.auth.origin().address_testnet();
                eprintln!("TX addr: {}", addr);
                if addr == alice_address {
                    alice_txs.insert(t.auth.get_origin_nonce(), r);
                } else if addr == bob_address {
                    bob_txs.insert(t.auth.get_origin_nonce(), r);
                }
            }
        }
    }

    assert_eq!(alice_txs.len(), 3, "Alice should have 3 confirmed txs");
    assert_eq!(bob_txs.len(), 2, "Bob should have 2 confirmed txs");

    assert!(
        match alice_txs.get(&0).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Alice tx0 should have committed okay"
    );

    assert!(
        match alice_txs.get(&1).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Alice tx1 should have committed okay"
    );

    assert_eq!(
        alice_txs.get(&2).unwrap().result,
        Value::err_none(),
        "Alice tx2 should have resulted in a runtime error (was the attempt to lock again in Pox 1)"
    );

    assert!(
        match bob_txs.get(&0).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Bob tx0 should have committed okay"
    );

    assert!(
        match bob_txs.get(&1).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Bob tx1 should have committed okay"
    );
}

/// In this test case, two Stackers, Alice and Bob delegate stack and interact with the
///  PoX v1 contract and PoX v2 contract across the epoch transition. This test
///  covers the two different ways a Stacker can be validly extended via `delegate-stack-extend` --
///  extending from a V1 lockup and extending from a V2 lockup.
///
/// Alice: delegate-stacks via PoX v1 for 4 cycles. The third of these cycles occurs after
///        the PoX v1 -> v2 transition, and so Alice gets "early unlocked".
///        Before the early unlock, Alice invokes:
///           `delegate-stx` in PoX v2
///           `delegate-stack-stx` in PoX v2
///        Alice tries to stack again via PoX v1, which is allowed by the contract,
///        but forbidden by the VM (because PoX has transitioned to v2)
/// Bob:   delegate-stacks via PoX v2 for 3 cycles.
///        Bob extends 1 cycles
#[test]
fn test_delegate_extend_transition_pox_2() {
    // this is the number of blocks after the first sortition any V1
    // PoX locks will automatically unlock at.
    let AUTO_UNLOCK_HT = 12;
    let EXPECTED_FIRST_V2_CYCLE = 8;
    // the sim environment produces 25 empty sortitions before
    //  tenures start being tracked.
    let EMPTY_SORTITIONS = 25;

    let mut burnchain = Burnchain::default_unittest(
        0,
        &BurnchainHeaderHash::from_hex(BITCOIN_REGTEST_FIRST_BLOCK_HASH).unwrap(),
    );
    burnchain.pox_constants.reward_cycle_length = 5;
    burnchain.pox_constants.prepare_length = 2;
    burnchain.pox_constants.anchor_threshold = 1;
    burnchain.pox_constants.v1_unlock_height = AUTO_UNLOCK_HT + EMPTY_SORTITIONS;

    let first_v2_cycle = burnchain
        .block_height_to_reward_cycle(burnchain.pox_constants.v1_unlock_height as u64)
        .unwrap()
        + 1;

    eprintln!("First v2 cycle = {}", first_v2_cycle);
    assert_eq!(first_v2_cycle, EXPECTED_FIRST_V2_CYCLE);

    let epochs = StacksEpoch::all(0, 0, EMPTY_SORTITIONS as u64 + 10);

    let observer = TestEventObserver::new();

    let (mut peer, mut keys) = instantiate_pox_peer_with_epoch(
        &burnchain,
        "test_delegate_extend_transition_pox_2",
        6002,
        Some(epochs.clone()),
        Some(&observer),
    );

    peer.config.check_pox_invariants =
        Some((EXPECTED_FIRST_V2_CYCLE, EXPECTED_FIRST_V2_CYCLE + 10));

    let num_blocks = 35;

    let alice = keys.pop().unwrap();
    let bob = keys.pop().unwrap();
    let charlie = keys.pop().unwrap();

    let alice_address = key_to_stacks_addr(&alice);
    let bob_address = key_to_stacks_addr(&bob);
    let charlie_address = key_to_stacks_addr(&charlie);

    let EXPECTED_ALICE_FIRST_REWARD_CYCLE = 6;
    let mut coinbase_nonce = 0;

    let INITIAL_BALANCE = 1024 * POX_THRESHOLD_STEPS_USTX;
    let LOCKUP_AMT = 1024 * POX_THRESHOLD_STEPS_USTX;

    // these checks should pass between Alice's first reward cycle,
    //  and the start of V2 reward cycles
    let alice_rewards_to_v2_start_checks = |tip_index_block, peer: &mut TestPeer| {
        let tip_burn_block_height = get_par_burn_block_height(peer.chainstate(), &tip_index_block);
        let cur_reward_cycle = burnchain
            .block_height_to_reward_cycle(tip_burn_block_height)
            .unwrap() as u128;
        let (min_ustx, reward_addrs, total_stacked) = with_sortdb(peer, |ref mut c, ref sortdb| {
            (
                c.get_stacking_minimum(sortdb, &tip_index_block).unwrap(),
                get_reward_addresses_with_par_tip(c, &burnchain, sortdb, &tip_index_block).unwrap(),
                c.test_get_total_ustx_stacked(sortdb, &tip_index_block, cur_reward_cycle)
                    .unwrap(),
            )
        });

        assert!(
            cur_reward_cycle >= EXPECTED_ALICE_FIRST_REWARD_CYCLE
                && cur_reward_cycle < first_v2_cycle as u128
        );
        // one reward address, and it's Charlies's
        // either way, there's a single reward address
        assert_eq!(reward_addrs.len(), 1);
        assert_eq!(
            (reward_addrs[0].0).version(),
            AddressHashMode::SerializeP2PKH as u8
        );
        assert_eq!(&(reward_addrs[0].0).hash160(), &charlie_address.bytes);
        // 1 lockup was done between alice's first cycle and the start of v2 cycles
        assert_eq!(reward_addrs[0].1, 1 * LOCKUP_AMT);
    };

    // these checks should pass after the start of V2 reward cycles
    let v2_rewards_checks = |tip_index_block, peer: &mut TestPeer| {
        let tip_burn_block_height = get_par_burn_block_height(peer.chainstate(), &tip_index_block);
        let cur_reward_cycle = burnchain
            .block_height_to_reward_cycle(tip_burn_block_height)
            .unwrap() as u128;
        let (min_ustx, reward_addrs, total_stacked) = with_sortdb(peer, |ref mut c, ref sortdb| {
            (
                c.get_stacking_minimum(sortdb, &tip_index_block).unwrap(),
                get_reward_addresses_with_par_tip(c, &burnchain, sortdb, &tip_index_block).unwrap(),
                c.test_get_total_ustx_stacked(sortdb, &tip_index_block, cur_reward_cycle)
                    .unwrap(),
            )
        });

        eprintln!(
            "reward_cycle = {}, reward_addrs = {}, total_stacked = {}",
            cur_reward_cycle,
            reward_addrs.len(),
            total_stacked
        );

        assert!(cur_reward_cycle >= first_v2_cycle as u128);
        // v2 reward cycles have begun, so reward addrs should be read from PoX2 which is just Charlie, but 2048*threshold
        assert_eq!(reward_addrs.len(), 1);
        assert_eq!(
            (reward_addrs[0].0).version(),
            AddressHashMode::SerializeP2PKH as u8
        );
        assert_eq!(&(reward_addrs[0].0).hash160(), &charlie_address.bytes);
        // 2 lockups were performed in v2 cycles
        assert_eq!(reward_addrs[0].1, 2 * LOCKUP_AMT);
    };

    // our "tenure counter" is now at 0
    let tip = get_tip(peer.sortdb.as_ref());
    assert_eq!(tip.block_height, 0 + EMPTY_SORTITIONS as u64);

    // first tenure is empty
    peer.tenure_with_txs(&[], &mut coinbase_nonce);

    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, INITIAL_BALANCE);

    let alice_account = get_account(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_account.stx_balance.amount_unlocked(), INITIAL_BALANCE,);
    assert_eq!(alice_account.stx_balance.amount_locked(), 0);
    assert_eq!(alice_account.stx_balance.unlock_height(), 0);

    // next tenure include Alice's lockup
    let tip = get_tip(peer.sortdb.as_ref());
    let delegate_tx = make_pox_contract_call(
        &alice,
        0,
        "delegate-stx",
        vec![
            Value::UInt(LOCKUP_AMT),
            PrincipalData::from(charlie_address.clone()).into(),
            Value::none(),
            Value::none(),
        ],
    );

    let delegate_stack_tx = make_pox_contract_call(
        &charlie,
        0,
        "delegate-stack-stx",
        vec![
            PrincipalData::from(alice_address.clone()).into(),
            Value::UInt(LOCKUP_AMT),
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(tip.block_height as u128),
            Value::UInt(4),
        ],
    );

    // aggregate commit to each cycle delegate-stack-stx locked for (cycles 6, 7, 8, 9)
    let agg_commit_tx_1 = make_pox_contract_call(
        &charlie,
        1,
        "stack-aggregation-commit",
        vec![
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(EXPECTED_ALICE_FIRST_REWARD_CYCLE),
        ],
    );

    let agg_commit_tx_2 = make_pox_contract_call(
        &charlie,
        2,
        "stack-aggregation-commit",
        vec![
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(EXPECTED_ALICE_FIRST_REWARD_CYCLE + 1),
        ],
    );

    let agg_commit_tx_3 = make_pox_contract_call(
        &charlie,
        3,
        "stack-aggregation-commit",
        vec![
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(EXPECTED_ALICE_FIRST_REWARD_CYCLE + 2),
        ],
    );

    let agg_commit_tx_4 = make_pox_contract_call(
        &charlie,
        4,
        "stack-aggregation-commit",
        vec![
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(EXPECTED_ALICE_FIRST_REWARD_CYCLE + 3),
        ],
    );

    // our "tenure counter" is now at 1
    assert_eq!(tip.block_height, 1 + EMPTY_SORTITIONS as u64);

    let tip_index_block = peer.tenure_with_txs(
        &[
            delegate_tx,
            delegate_stack_tx,
            agg_commit_tx_1,
            agg_commit_tx_2,
            agg_commit_tx_3,
            agg_commit_tx_4,
        ],
        &mut coinbase_nonce,
    );

    // check the stacking minimum
    let total_liquid_ustx = get_liquid_ustx(&mut peer);
    let min_ustx = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
        chainstate.get_stacking_minimum(sortdb, &tip_index_block)
    })
    .unwrap();
    assert_eq!(
        min_ustx,
        total_liquid_ustx / POX_TESTNET_STACKING_THRESHOLD_25
    );

    // no reward addresses
    let reward_addrs = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
        get_reward_addresses_with_par_tip(chainstate, &burnchain, sortdb, &tip_index_block)
    })
    .unwrap();
    assert_eq!(reward_addrs.len(), 0);

    // check the first reward cycle when Alice's tokens get stacked
    let tip_burn_block_height = get_par_burn_block_height(peer.chainstate(), &tip_index_block);
    let alice_first_reward_cycle = 1 + burnchain
        .block_height_to_reward_cycle(tip_burn_block_height)
        .unwrap() as u128;

    assert_eq!(alice_first_reward_cycle, EXPECTED_ALICE_FIRST_REWARD_CYCLE);

    // alice locked, so balance should be 0
    let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
    assert_eq!(alice_balance, 0);

    // produce blocks until alice's first reward cycle
    for _i in 0..4 {
        peer.tenure_with_txs(&[], &mut coinbase_nonce);
    }

    // produce blocks until immediately after the epoch switch (8 more blocks to block height 36)
    for _i in 0..4 {
        let tip_index_block = peer.tenure_with_txs(&[], &mut coinbase_nonce);

        // alice is still locked, balance should be 0
        let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
        assert_eq!(alice_balance, 0);

        alice_rewards_to_v2_start_checks(tip_index_block, &mut peer);
    }

    // in the next tenure, PoX 2 should now exist.
    // Lets have Bob lock up for v2
    // this will lock for cycles 8, 9, 10
    //  the first v2 cycle will be 8
    let tip = get_tip(peer.sortdb.as_ref());

    let bob_delegate_tx = make_pox_2_contract_call(
        &bob,
        0,
        "delegate-stx",
        vec![
            Value::UInt(2048 * POX_THRESHOLD_STEPS_USTX),
            PrincipalData::from(charlie_address.clone()).into(),
            Value::none(),
            Value::none(),
        ],
    );

    let alice_delegate_tx = make_pox_2_contract_call(
        &alice,
        1,
        "delegate-stx",
        vec![
            Value::UInt(2048 * POX_THRESHOLD_STEPS_USTX),
            PrincipalData::from(charlie_address.clone()).into(),
            Value::none(),
            Value::none(),
        ],
    );

    let delegate_stack_tx = make_pox_2_contract_call(
        &charlie,
        5,
        "delegate-stack-stx",
        vec![
            PrincipalData::from(bob_address.clone()).into(),
            Value::UInt(LOCKUP_AMT),
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(tip.block_height as u128),
            Value::UInt(3),
        ],
    );

    // Alice _will_ auto-unlock: she can be delegate-stack-extend'ed in PoX v2
    let delegate_extend_tx = make_pox_2_contract_call(
        &charlie,
        6,
        "delegate-stack-extend",
        vec![
            PrincipalData::from(alice_address.clone()).into(),
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(6),
        ],
    );

    // Charlie agg commits the first 3 cycles, but wait until delegate-extended bob to
    //   agg commit the 4th cycle
    // aggregate commit to each cycle delegate-stack-stx locked for (cycles 6, 7, 8, 9)
    let agg_commit_tx_1 = make_pox_2_contract_call(
        &charlie,
        7,
        "stack-aggregation-commit",
        vec![
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(first_v2_cycle as u128),
        ],
    );

    let agg_commit_tx_2 = make_pox_2_contract_call(
        &charlie,
        8,
        "stack-aggregation-commit",
        vec![
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(first_v2_cycle as u128 + 1),
        ],
    );

    let agg_commit_tx_3 = make_pox_2_contract_call(
        &charlie,
        9,
        "stack-aggregation-commit",
        vec![
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(first_v2_cycle as u128 + 2),
        ],
    );

    // our "tenure counter" is now at 10
    assert_eq!(tip.block_height, 10 + EMPTY_SORTITIONS as u64);

    let tip_index_block = peer.tenure_with_txs(
        &[
            bob_delegate_tx,
            alice_delegate_tx,
            delegate_stack_tx,
            delegate_extend_tx,
            agg_commit_tx_1,
            agg_commit_tx_2,
            agg_commit_tx_3,
        ],
        &mut coinbase_nonce,
    );
    alice_rewards_to_v2_start_checks(tip_index_block, &mut peer);
    let alice_principal = alice_address.clone().into();
    let bob_principal = bob_address.clone().into();
    let StackingStateCheckData {
        first_cycle: alice_first_cycle,
        lock_period: alice_lock_period,
        ..
    } = check_stacking_state_invariants(&mut peer, &tip_index_block, &alice_principal, false);
    let StackingStateCheckData {
        first_cycle: bob_first_cycle,
        lock_period: bob_lock_period,
        ..
    } = check_stacking_state_invariants(&mut peer, &tip_index_block, &bob_principal, false);

    assert_eq!(
        alice_first_cycle as u64, first_v2_cycle,
        "Alice's first cycle in PoX-2 stacking state is the next cycle, which is 8"
    );
    assert_eq!(alice_lock_period, 6);
    assert_eq!(
        bob_first_cycle as u64, first_v2_cycle,
        "Bob's first cycle in PoX-2 stacking state is the next cycle, which is 8"
    );
    assert_eq!(bob_lock_period, 3);

    // Extend bob's lockup via `delegate-stack-extend` for 1 more cycle
    let delegate_extend_tx = make_pox_2_contract_call(
        &charlie,
        10,
        "delegate-stack-extend",
        vec![
            PrincipalData::from(bob_address.clone()).into(),
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(1),
        ],
    );

    let agg_commit_tx = make_pox_2_contract_call(
        &charlie,
        11,
        "stack-aggregation-commit",
        vec![
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(first_v2_cycle as u128 + 3),
        ],
    );

    let tip_index_block =
        peer.tenure_with_txs(&[delegate_extend_tx, agg_commit_tx], &mut coinbase_nonce);
    alice_rewards_to_v2_start_checks(tip_index_block, &mut peer);
    let StackingStateCheckData {
        first_cycle: alice_first_cycle,
        lock_period: alice_lock_period,
        ..
    } = check_stacking_state_invariants(&mut peer, &tip_index_block, &alice_principal, false);
    let StackingStateCheckData {
        first_cycle: bob_first_cycle,
        lock_period: bob_lock_period,
        ..
    } = check_stacking_state_invariants(&mut peer, &tip_index_block, &bob_principal, false);

    assert_eq!(
        alice_first_cycle as u64, first_v2_cycle,
        "Alice's first cycle in PoX-2 stacking state is the next cycle, which is 8"
    );
    assert_eq!(alice_lock_period, 6);
    assert_eq!(
        bob_first_cycle as u64, first_v2_cycle,
        "Bob's first cycle in PoX-2 stacking state is the next cycle, which is 8"
    );
    assert_eq!(bob_lock_period, 4);

    // produce blocks until "tenure counter" is 15 -- this is where
    //  the v2 reward cycles start
    for _i in 0..3 {
        let tip_index_block = peer.tenure_with_txs(&[], &mut coinbase_nonce);

        // alice is still locked, balance should be 0
        let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
        assert_eq!(alice_balance, 0);

        alice_rewards_to_v2_start_checks(tip_index_block, &mut peer);
    }

    let tip = get_tip(peer.sortdb.as_ref());
    // our "tenure counter" is now at 15
    assert_eq!(tip.block_height, 15 + EMPTY_SORTITIONS as u64);

    // Extend bob's lockup via `delegate-stack-extend` for 1 more cycle
    //  so that we can check the first-reward-cycle is correctly updated
    let delegate_extend_tx = make_pox_2_contract_call(
        &charlie,
        12,
        "delegate-stack-extend",
        vec![
            PrincipalData::from(bob_address.clone()).into(),
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(1),
        ],
    );

    let tip_index_block = peer.tenure_with_txs(&[delegate_extend_tx], &mut coinbase_nonce);
    v2_rewards_checks(tip_index_block, &mut peer);
    let StackingStateCheckData {
        first_cycle: alice_first_cycle,
        lock_period: alice_lock_period,
        ..
    } = check_stacking_state_invariants(&mut peer, &tip_index_block, &alice_principal, false);
    let StackingStateCheckData {
        first_cycle: bob_first_cycle,
        lock_period: bob_lock_period,
        ..
    } = check_stacking_state_invariants(&mut peer, &tip_index_block, &bob_principal, false);

    assert_eq!(
        alice_first_cycle as u64, first_v2_cycle,
        "Alice's first cycle in PoX-2 stacking state is the next cycle, which is 8"
    );
    assert_eq!(alice_lock_period, 6);
    assert_eq!(
        bob_first_cycle as u64, first_v2_cycle,
        "Bob's first cycle in PoX-2 stacking state is the next cycle, which is 8"
    );
    assert_eq!(bob_lock_period, 5);

    // produce blocks until "tenure counter" is 32 -- this is where
    //  alice *would have been* unlocked under v1 rules
    for _i in 0..16 {
        let tip_index_block = peer.tenure_with_txs(&[], &mut coinbase_nonce);

        // alice is still locked, balance should be 0
        let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
        assert_eq!(alice_balance, 0);

        v2_rewards_checks(tip_index_block, &mut peer);
    }

    // our "tenure counter" is now at 32
    let tip = get_tip(peer.sortdb.as_ref());
    assert_eq!(tip.block_height, 32 + EMPTY_SORTITIONS as u64);

    // Alice would have unlocked under v1 rules, so try to stack again via PoX 1 and expect a runtime error
    // in the tx
    let alice_lockup = make_pox_lockup(
        &alice,
        2,
        512 * POX_THRESHOLD_STEPS_USTX,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        12,
        tip.block_height,
    );

    let tip_index_block = peer.tenure_with_txs(&[alice_lockup], &mut coinbase_nonce);
    v2_rewards_checks(tip_index_block, &mut peer);

    // now let's check some tx receipts
    let blocks = observer.get_blocks();

    let mut alice_txs = HashMap::new();
    let mut bob_txs = HashMap::new();
    let mut charlie_txs = HashMap::new();

    for b in blocks.into_iter() {
        for r in b.receipts.into_iter() {
            if let TransactionOrigin::Stacks(ref t) = r.transaction {
                let addr = t.auth.origin().address_testnet();
                eprintln!("TX addr: {}", addr);
                if addr == alice_address {
                    alice_txs.insert(t.auth.get_origin_nonce(), r);
                } else if addr == bob_address {
                    bob_txs.insert(t.auth.get_origin_nonce(), r);
                } else if addr == charlie_address {
                    charlie_txs.insert(t.auth.get_origin_nonce(), r);
                }
            }
        }
    }

    assert_eq!(alice_txs.len(), 3, "Alice should have 3 confirmed txs");
    assert_eq!(bob_txs.len(), 1, "Bob should have 1 confirmed tx");
    assert_eq!(
        charlie_txs.len(),
        13,
        "Charlie should have 13 confirmed txs"
    );

    assert!(
        match alice_txs.get(&0).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Alice tx0 should have committed okay"
    );

    assert!(
        match alice_txs.get(&1).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Alice tx1 should have committed okay"
    );

    assert_eq!(
        alice_txs.get(&2).unwrap().result,
        Value::err_none(),
        "Alice tx2 should have resulted in a runtime error (was the attempt to lock again in Pox 1)"
    );

    assert!(
        match bob_txs.get(&0).unwrap().result {
            Value::Response(ref r) => r.committed,
            _ => false,
        },
        "Bob tx0 should have committed okay"
    );

    for (_nonce, tx) in charlie_txs.iter() {
        assert!(
            match tx.result {
                Value::Response(ref r) => r.committed,
                _ => false,
            },
            "All of Charlie's transactions should have committed okay"
        );
    }
}

#[test]
fn test_pox_2_getters() {
    // this is the number of blocks after the first sortition any V1
    // PoX locks will automatically unlock at.
    let AUTO_UNLOCK_HT = 12;
    let EXPECTED_FIRST_V2_CYCLE = 8;
    // the sim environment produces 25 empty sortitions before
    //  tenures start being tracked.
    let EMPTY_SORTITIONS = 25;
    let LOCKUP_AMT = 1024 * POX_THRESHOLD_STEPS_USTX;

    let mut burnchain = Burnchain::default_unittest(
        0,
        &BurnchainHeaderHash::from_hex(BITCOIN_REGTEST_FIRST_BLOCK_HASH).unwrap(),
    );
    burnchain.pox_constants.reward_cycle_length = 5;
    burnchain.pox_constants.prepare_length = 2;
    burnchain.pox_constants.anchor_threshold = 1;
    burnchain.pox_constants.v1_unlock_height = AUTO_UNLOCK_HT + EMPTY_SORTITIONS;

    let first_v2_cycle = burnchain
        .block_height_to_reward_cycle(burnchain.pox_constants.v1_unlock_height as u64)
        .unwrap()
        + 1;

    eprintln!("First v2 cycle = {}", first_v2_cycle);
    assert_eq!(first_v2_cycle, EXPECTED_FIRST_V2_CYCLE);

    let epochs = StacksEpoch::all(0, 0, EMPTY_SORTITIONS as u64 + 10);

    let (mut peer, mut keys) = instantiate_pox_peer_with_epoch(
        &burnchain,
        "test-pox-2-getters",
        6100,
        Some(epochs.clone()),
        None,
    );

    peer.config.check_pox_invariants =
        Some((EXPECTED_FIRST_V2_CYCLE, EXPECTED_FIRST_V2_CYCLE + 10));

    let mut coinbase_nonce = 0;
    let alice = keys.pop().unwrap();
    let bob = keys.pop().unwrap();
    let charlie = keys.pop().unwrap();
    let danielle = keys.pop().unwrap();

    let alice_address = key_to_stacks_addr(&alice);
    let bob_address = key_to_stacks_addr(&bob);
    let charlie_address = key_to_stacks_addr(&charlie);

    for _i in 0..20 {
        peer.tenure_with_txs(&[], &mut coinbase_nonce);
    }

    let tip = get_tip(peer.sortdb.as_ref());
    let cur_reward_cycle = burnchain
        .block_height_to_reward_cycle(tip.block_height)
        .unwrap();

    // alice locks in v2
    let alice_lockup = make_pox_2_lockup(
        &alice,
        0,
        LOCKUP_AMT,
        AddressHashMode::SerializeP2PKH,
        key_to_stacks_addr(&alice).bytes,
        4,
        tip.block_height,
    );

    // bob deleates to charlie
    let bob_delegate_tx = make_pox_2_contract_call(
        &bob,
        0,
        "delegate-stx",
        vec![
            Value::UInt(LOCKUP_AMT),
            PrincipalData::from(charlie_address.clone()).into(),
            Value::none(),
            Value::none(),
        ],
    );

    // charlie calls delegate-stack-stx for bob
    let charlie_delegate_stack_tx = make_pox_2_contract_call(
        &charlie,
        0,
        "delegate-stack-stx",
        vec![
            PrincipalData::from(bob_address.clone()).into(),
            Value::UInt(LOCKUP_AMT),
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(tip.block_height as u128),
            Value::UInt(4),
        ],
    );

    let agg_commit_tx_1 = make_pox_2_contract_call(
        &charlie,
        1,
        "stack-aggregation-commit",
        vec![
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(cur_reward_cycle as u128),
        ],
    );

    let agg_commit_tx_2 = make_pox_2_contract_call(
        &charlie,
        2,
        "stack-aggregation-commit",
        vec![
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(cur_reward_cycle as u128 + 1),
        ],
    );

    let agg_commit_tx_3 = make_pox_2_contract_call(
        &charlie,
        3,
        "stack-aggregation-commit",
        vec![
            make_pox_addr(
                AddressHashMode::SerializeP2PKH,
                charlie_address.bytes.clone(),
            ),
            Value::UInt(cur_reward_cycle as u128 + 2),
        ],
    );

    let reject_pox = make_pox_2_contract_call(&danielle, 0, "reject-pox", vec![]);

    peer.tenure_with_txs(
        &[
            alice_lockup,
            bob_delegate_tx,
            charlie_delegate_stack_tx,
            agg_commit_tx_1,
            agg_commit_tx_2,
            agg_commit_tx_3,
            reject_pox,
        ],
        &mut coinbase_nonce,
    );

    let result = eval_at_tip(&mut peer, "pox-2", &format!("
    {{
        ;; should be none
        get-delegation-info-alice: (get-delegation-info '{}),
        ;; should be (some $charlie_address)
        get-delegation-info-bob: (get-delegation-info '{}),
        ;; should be none
        get-allowance-contract-callers: (get-allowance-contract-callers '{} '{}),
        ;; should be 1
        get-num-reward-set-pox-addresses-current: (get-num-reward-set-pox-addresses u{}),
        ;; should be 0
        get-num-reward-set-pox-addresses-future: (get-num-reward-set-pox-addresses u1000),
        ;; should be 0
        get-partial-stacked-by-cycle-bob-0: (get-partial-stacked-by-cycle {{ version: 0x00, hashbytes: 0x{} }} u{} '{}),
        get-partial-stacked-by-cycle-bob-1: (get-partial-stacked-by-cycle {{ version: 0x00, hashbytes: 0x{} }} u{} '{}),
        get-partial-stacked-by-cycle-bob-2: (get-partial-stacked-by-cycle {{ version: 0x00, hashbytes: 0x{} }} u{} '{}),
        ;; should be LOCKUP_AMT
        get-partial-stacked-by-cycle-bob-3: (get-partial-stacked-by-cycle {{ version: 0x00, hashbytes: 0x{} }} u{} '{}),
        ;; should be 0
        get-total-pox-rejection-now: (get-total-pox-rejection u{}),
        ;; should be LOCKUP_AMT
        get-total-pox-rejection-next: (get-total-pox-rejection u{}),
        ;; should be 0
        get-total-pox-rejection-future: (get-total-pox-rejection u{})
    }}", &alice_address,
        &bob_address,
        &bob_address, &format!("{}.hello-world", &charlie_address), cur_reward_cycle + 1,
        &charlie_address.bytes, cur_reward_cycle + 0, &charlie_address,
        &charlie_address.bytes, cur_reward_cycle + 1, &charlie_address,
        &charlie_address.bytes, cur_reward_cycle + 2, &charlie_address,
        &charlie_address.bytes, cur_reward_cycle + 3, &charlie_address,
        cur_reward_cycle,
        cur_reward_cycle + 1,
        cur_reward_cycle + 2,
    ));

    eprintln!("{}", &result);
    let data = result.expect_tuple().data_map;

    let alice_delegation_info = data
        .get("get-delegation-info-alice")
        .cloned()
        .unwrap()
        .expect_optional();
    assert!(alice_delegation_info.is_none());

    let bob_delegation_info = data
        .get("get-delegation-info-bob")
        .cloned()
        .unwrap()
        .expect_optional()
        .unwrap()
        .expect_tuple()
        .data_map;
    let bob_delegation_addr = bob_delegation_info
        .get("delegated-to")
        .cloned()
        .unwrap()
        .expect_principal();
    let bob_delegation_amt = bob_delegation_info
        .get("amount-ustx")
        .cloned()
        .unwrap()
        .expect_u128();
    let bob_pox_addr_opt = bob_delegation_info
        .get("pox-addr")
        .cloned()
        .unwrap()
        .expect_optional();
    assert_eq!(bob_delegation_addr, charlie_address.to_account_principal());
    assert_eq!(bob_delegation_amt, LOCKUP_AMT as u128);
    assert!(bob_pox_addr_opt.is_none());

    let allowance = data
        .get("get-allowance-contract-callers")
        .cloned()
        .unwrap()
        .expect_optional();
    assert!(allowance.is_none());

    let current_num_reward_addrs = data
        .get("get-num-reward-set-pox-addresses-current")
        .cloned()
        .unwrap()
        .expect_u128();
    assert_eq!(current_num_reward_addrs, 2);

    let future_num_reward_addrs = data
        .get("get-num-reward-set-pox-addresses-future")
        .cloned()
        .unwrap()
        .expect_u128();
    assert_eq!(future_num_reward_addrs, 0);

    for i in 0..3 {
        let key =
            ClarityName::try_from(format!("get-partial-stacked-by-cycle-bob-{}", &i)).unwrap();
        let partial_stacked = data.get(&key).cloned().unwrap().expect_optional();
        assert!(partial_stacked.is_none());
    }
    let partial_stacked = data
        .get("get-partial-stacked-by-cycle-bob-3")
        .cloned()
        .unwrap()
        .expect_optional()
        .unwrap()
        .expect_tuple()
        .data_map
        .get("stacked-amount")
        .cloned()
        .unwrap()
        .expect_u128();
    assert_eq!(partial_stacked, LOCKUP_AMT as u128);

    let rejected = data
        .get("get-total-pox-rejection-now")
        .cloned()
        .unwrap()
        .expect_u128();
    assert_eq!(rejected, 0);

    let rejected = data
        .get("get-total-pox-rejection-next")
        .cloned()
        .unwrap()
        .expect_u128();
    assert_eq!(rejected, LOCKUP_AMT as u128);

    let rejected = data
        .get("get-total-pox-rejection-future")
        .cloned()
        .unwrap()
        .expect_u128();
    assert_eq!(rejected, 0);
}

#[test]
fn test_get_pox_addrs() {
    let mut burnchain = Burnchain::default_unittest(0, &BurnchainHeaderHash::zero());
    burnchain.pox_constants.reward_cycle_length = 4; // 4 reward slots
    burnchain.pox_constants.prepare_length = 2;
    burnchain.pox_constants.anchor_threshold = 1;
    burnchain.pox_constants.v1_unlock_height = 4;

    assert_eq!(burnchain.pox_constants.reward_slots(), 4);

    let epochs = StacksEpoch::all(1, 2, 3);

    let (mut peer, keys) = instantiate_pox_peer_with_epoch(
        &burnchain,
        "test-get-pox-addrs",
        6102,
        Some(epochs.clone()),
        None,
    );
    let num_blocks = 20;

    let mut lockup_reward_cycle = 0;
    let mut prepared = false;
    let mut rewarded = false;
    let mut paid_out = HashSet::new();
    let mut all_reward_addrs = vec![];

    for tenure_id in 0..num_blocks {
        let microblock_privkey = StacksPrivateKey::new();
        let microblock_pubkeyhash =
            Hash160::from_node_public_key(&StacksPublicKey::from_private(&microblock_privkey));
        let tip = SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn())
            .unwrap();

        let cur_reward_cycle = burnchain
            .block_height_to_reward_cycle(tip.block_height)
            .unwrap() as u128;

        let (burn_ops, stacks_block, microblocks) = peer.make_tenure(
            |ref mut miner,
             ref mut sortdb,
             ref mut chainstate,
             vrf_proof,
             ref parent_opt,
             ref parent_microblock_header_opt| {
                let parent_tip = get_parent_tip(parent_opt, chainstate, sortdb);
                let coinbase_tx = make_coinbase(miner, tenure_id);

                let mut block_txs = vec![coinbase_tx];

                if tenure_id == 1 {
                    // all peers lock at the same time
                    for (key, hash_mode) in keys.iter().zip(
                        [
                            AddressHashMode::SerializeP2PKH,
                            AddressHashMode::SerializeP2SH,
                            AddressHashMode::SerializeP2WPKH,
                            AddressHashMode::SerializeP2WSH,
                        ]
                        .iter(),
                    ) {
                        let lockup = make_pox_2_lockup(
                            key,
                            0,
                            1024 * POX_THRESHOLD_STEPS_USTX,
                            *hash_mode,
                            key_to_stacks_addr(key).bytes,
                            2,
                            tip.block_height,
                        );
                        block_txs.push(lockup);
                    }
                }

                let block_builder = StacksBlockBuilder::make_block_builder(
                    false,
                    &parent_tip,
                    vrf_proof,
                    tip.total_burn,
                    microblock_pubkeyhash,
                )
                .unwrap();
                let (anchored_block, _size, _cost) =
                    StacksBlockBuilder::make_anchored_block_from_txs(
                        block_builder,
                        chainstate,
                        &sortdb.index_conn(),
                        block_txs,
                    )
                    .unwrap();
                (anchored_block, vec![])
            },
        );

        let (burn_height, _, consensus_hash) = peer.next_burnchain_block(burn_ops.clone());
        peer.process_stacks_epoch_at_tip(&stacks_block, &microblocks);

        if burnchain.is_in_prepare_phase(burn_height)
            || burn_height >= burnchain.reward_cycle_to_block_height(lockup_reward_cycle + 2)
        {
            // make sure we burn!
            for op in burn_ops.iter() {
                if let BlockstackOperationType::LeaderBlockCommit(ref opdata) = &op {
                    eprintln!("prepare phase || no PoX {}: {:?}", burn_height, opdata);
                    assert!(opdata.all_outputs_burn());
                    assert!(opdata.burn_fee > 0);

                    if tenure_id > 1 && cur_reward_cycle > lockup_reward_cycle.into() {
                        prepared = true;
                    }
                }
            }
        } else {
            // no burns -- 100% commitment
            for op in burn_ops.iter() {
                if let BlockstackOperationType::LeaderBlockCommit(ref opdata) = &op {
                    eprintln!("reward phase {}: {:?}", burn_height, opdata);
                    if tenure_id > 1 && cur_reward_cycle == (lockup_reward_cycle + 1).into() {
                        assert!(!opdata.all_outputs_burn());
                        rewarded = true;
                    } else {
                        // lockup hasn't happened yet
                        assert!(opdata.all_outputs_burn());
                    }

                    assert!(opdata.burn_fee > 0);
                }
            }
        }

        let total_liquid_ustx = get_liquid_ustx(&mut peer);
        let tip_index_block = StacksBlockId::new(&consensus_hash, &stacks_block.block_hash());

        let tip_burn_block_height = get_par_burn_block_height(peer.chainstate(), &tip_index_block);
        let cur_reward_cycle = burnchain
            .block_height_to_reward_cycle(tip_burn_block_height)
            .unwrap() as u128;

        if tenure_id <= 1 {
            // record the first reward cycle when tokens get stacked
            lockup_reward_cycle = 1
                + (burnchain
                    .block_height_to_reward_cycle(tip_burn_block_height)
                    .unwrap()) as u64;
            eprintln!(
                "\nlockup reward cycle: {}\ncur reward cycle: {}\n",
                lockup_reward_cycle, cur_reward_cycle
            );
        }
        if tenure_id > 1 {
            let min_ustx = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
                chainstate.get_stacking_minimum(sortdb, &tip_index_block)
            })
            .unwrap();
            let reward_addrs = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
                get_reward_addresses_with_par_tip(chainstate, &burnchain, sortdb, &tip_index_block)
            })
            .unwrap();
            let total_stacked = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
                chainstate.test_get_total_ustx_stacked(sortdb, &tip_index_block, cur_reward_cycle)
            })
            .unwrap();

            // all keys locked up STX no matter what if we're in the lock period
            if burn_height < burnchain.reward_cycle_to_block_height(lockup_reward_cycle + 2) {
                for key in keys.iter() {
                    let balance = get_balance(&mut peer, &key_to_stacks_addr(key).into());
                    assert_eq!(balance, 0);
                }
            } else {
                for key in keys.iter() {
                    let balance = get_balance(&mut peer, &key_to_stacks_addr(key).into());
                    assert!(balance > 0);
                }
                assert_eq!(reward_addrs.len(), 0);
            }

            eprintln!("\ntenure: {}\nreward cycle: {}\nlockup_reward_cycle: {}\nmin-uSTX: {}\naddrs: {:?}\ntotal_liquid_ustx: {}\ntotal-stacked: {}\n", tenure_id, cur_reward_cycle, lockup_reward_cycle, min_ustx, &reward_addrs, total_liquid_ustx, total_stacked);

            if cur_reward_cycle == lockup_reward_cycle.into() {
                assert_eq!(reward_addrs.len(), 4);
                all_reward_addrs = reward_addrs;
            }

            // let's see who got paid
            let addrs_and_payout = with_sortdb(&mut peer, |ref mut chainstate, ref mut sortdb| {
                let addrs = chainstate
                    .maybe_read_only_clarity_tx(
                        &sortdb.index_conn(),
                        &tip_index_block,
                        |clarity_tx| {
                            clarity_tx
                                .with_readonly_clarity_env(
                                    false,
                                    0x80000000,
                                    ClarityVersion::Clarity2,
                                    PrincipalData::Standard(StandardPrincipalData::transient()),
                                    None,
                                    LimitedCostTracker::new_free(),
                                    |env| {
                                        env.eval_read_only(
                                            &boot_code_id("pox-2", false),
                                            &format!(
                                                "(get-burn-block-info? pox-addrs u{})",
                                                &(burn_height - 1)
                                            ),
                                        )
                                    },
                                )
                                .unwrap()
                        },
                    )
                    .unwrap();
                addrs
            })
            .unwrap()
            .expect_optional()
            .expect("FATAL: expected list")
            .expect_tuple();

            eprintln!(
                "At block height {}: {:?}",
                burn_height - 1,
                &addrs_and_payout
            );

            let addrs = addrs_and_payout
                .get("addrs")
                .unwrap()
                .to_owned()
                .expect_list();

            let payout = addrs_and_payout
                .get("payout")
                .unwrap()
                .to_owned()
                .expect_u128();

            // there's always some burnchain tokens spent.
            assert!(payout > 0);

            if burnchain.is_in_prepare_phase(burn_height - 1) {
                assert_eq!(payout, 1000);
                assert_eq!(addrs.len(), 1);
                let pox_addr = PoxAddress::try_from_pox_tuple(false, &addrs[0]).unwrap();
                assert!(pox_addr.is_burn());
            } else {
                assert_eq!(payout, 500);
                assert_eq!(addrs.len(), 2);
                for addr in addrs.into_iter() {
                    let pox_addr = PoxAddress::try_from_pox_tuple(false, &addr).unwrap();
                    if !pox_addr.is_burn() {
                        paid_out.insert(pox_addr);
                    }
                }
            }
        }
    }
    assert!(prepared);
    assert!(rewarded);

    assert_eq!(paid_out.len(), 4);
    for (rw_addr, _) in all_reward_addrs.into_iter() {
        assert!(paid_out.contains(&rw_addr));
    }
}