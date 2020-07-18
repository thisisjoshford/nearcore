use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use log::info;

use near_epoch_manager::EpochManager;
use near_primitives::errors::EpochError;
use near_primitives::hash::CryptoHash;
use near_primitives::types::{AccountId, EpochId, ShardId};

const POISONED_LOCK_ERR: &str = "The lock was poisoned.";

/// Tracker that tracks shard ids and accounts. It maintains two items: `tracked_accounts` and
/// `tracked_shards`. The shards that are actually tracked are the union of shards that `tracked_accounts`
/// are in and `tracked_shards`.
#[derive(Clone)]
pub struct ShardTracker {
    /// Tracked accounts by shard id. For each shard id, the corresponding set of accounts should be
    /// non empty (otherwise the entry should not exist).
    tracked_accounts: HashMap<ShardId, HashSet<AccountId>>,
    /// Tracked shards.
    tracked_shards: HashSet<ShardId>,
    /// Combination of shards that correspond to tracked accounts and tracked shards.
    actual_tracked_shards: HashSet<ShardId>,
    /// Accounts that we stop tracking in the next epoch.
    pending_untracked_accounts: HashSet<AccountId>,
    /// Shards that we stop tracking in the next epoch.
    pending_untracked_shards: HashSet<ShardId>,
    /// Current epoch id. Used to determine whether we need to flush pending requests.
    current_epoch_id: EpochId,
    /// Epoch manager that for given block hash computes the epoch id.
    epoch_manager: Arc<RwLock<EpochManager>>,
}

impl ShardTracker {
    pub fn new(
        accounts: Vec<AccountId>,
        shards: Vec<ShardId>,
        prev_block_hash: CryptoHash,
        epoch_id: EpochId,
        epoch_manager: Arc<RwLock<EpochManager>>,
    ) -> Self {
        let tracked_accounts = accounts.into_iter().fold(HashMap::new(), |mut acc, x| {
            let shard_id = {
                let mut epoch_manager = epoch_manager.write().expect(POISONED_LOCK_ERR);
                epoch_manager.account_id_to_shard_id(&x, &prev_block_hash).unwrap()
            };
            acc.entry(shard_id).or_insert_with(HashSet::new).insert(x);
            acc
        });
        let tracked_shards: HashSet<_> = shards.into_iter().collect();
        let mut actual_tracked_shards = tracked_shards.clone();
        for (shard_id, _) in tracked_accounts.iter() {
            actual_tracked_shards.insert(*shard_id);
        }
        info!(target: "runtime", "Tracking shards: {:?}", actual_tracked_shards);
        ShardTracker {
            tracked_accounts,
            tracked_shards,
            actual_tracked_shards,
            pending_untracked_accounts: HashSet::default(),
            pending_untracked_shards: HashSet::default(),
            current_epoch_id: epoch_id,
            epoch_manager,
        }
    }

    fn track_account(
        &mut self,
        account_id: &AccountId,
        prev_block_hash: &CryptoHash,
    ) -> Result<(), EpochError> {
        let mut epoch_manager = self.epoch_manager.write().expect(POISONED_LOCK_ERR);
        let shard_id = epoch_manager.account_id_to_shard_id(account_id, prev_block_hash)?;
        self.tracked_accounts
            .entry(shard_id)
            .or_insert_with(HashSet::new)
            .insert(account_id.clone());
        self.actual_tracked_shards.insert(shard_id);
        Ok(())
    }

    /// Track a list of accounts. The tracking will take effect immediately because
    /// even if we want to start tracking the accounts in the next epoch, it cannot harm
    /// us to start tracking them earlier.
    #[allow(unused)]
    pub fn track_accounts(&mut self, account_ids: &[AccountId], prev_block_hash: &CryptoHash) {
        for account_id in account_ids.iter() {
            self.track_account(account_id, prev_block_hash);
        }
    }

    fn track_shard(&mut self, shard_id: ShardId) {
        self.tracked_shards.insert(shard_id);
        self.actual_tracked_shards.insert(shard_id);
    }

    /// Track a list of shards. Similar to tracking accounts, the tracking starts immediately.
    #[allow(unused)]
    pub fn track_shards(&mut self, shard_ids: &[ShardId]) {
        for shard_id in shard_ids.iter() {
            self.track_shard(*shard_id);
        }
    }

    fn flush_pending(&mut self, prev_block_hash: &CryptoHash) -> Result<(), EpochError> {
        let mut epoch_manager = self.epoch_manager.write().expect(POISONED_LOCK_ERR);
        let mut shards_to_remove = HashSet::new();
        for account_id in self.pending_untracked_accounts.iter() {
            let shard_id = epoch_manager.account_id_to_shard_id(&account_id, prev_block_hash)?;
            self.tracked_accounts.entry(shard_id).and_modify(|e| {
                e.remove(account_id);
            });
            let to_remove = if let Some(accounts) = self.tracked_accounts.get(&shard_id) {
                accounts.is_empty()
            } else {
                false
            };
            if to_remove {
                self.tracked_accounts.remove(&shard_id);
                shards_to_remove.insert(shard_id);
            }
        }
        self.pending_untracked_accounts.clear();
        for shard_id in self.pending_untracked_shards.drain() {
            self.tracked_shards.remove(&shard_id);
            shards_to_remove.insert(shard_id);
        }
        for shard_id in shards_to_remove.drain() {
            if !self.tracked_accounts.contains_key(&shard_id)
                && !self.tracked_shards.contains(&shard_id)
            {
                self.actual_tracked_shards.remove(&shard_id);
            }
        }
        Ok(())
    }

    fn update_epoch(&mut self, block_hash: &CryptoHash) -> Result<(), EpochError> {
        let epoch_id = {
            let mut epoch_manager = self.epoch_manager.write().expect(POISONED_LOCK_ERR);
            epoch_manager.get_epoch_id(block_hash)?
        };
        if self.current_epoch_id != epoch_id {
            // if epoch id has changed, we need to flush the pending removals
            // and update the shards to track
            self.flush_pending(block_hash)?;
            self.current_epoch_id = epoch_id;
        }
        Ok(())
    }

    /// Stop tracking a list of accounts in the next epoch.
    #[allow(unused)]
    pub fn untrack_accounts(
        &mut self,
        block_hash: &CryptoHash,
        account_ids: Vec<AccountId>,
    ) -> Result<(), EpochError> {
        self.update_epoch(block_hash)?;
        for account_id in account_ids {
            self.pending_untracked_accounts.insert(account_id);
        }
        Ok(())
    }

    /// Stop tracking a list of shards in the next epoch.
    #[allow(unused)]
    pub fn untrack_shards(
        &mut self,
        block_hash: &CryptoHash,
        shard_ids: Vec<ShardId>,
    ) -> Result<(), EpochError> {
        self.update_epoch(block_hash)?;
        for shard_id in shard_ids {
            self.pending_untracked_shards.insert(shard_id);
        }
        Ok(())
    }

    pub fn care_about_shard(
        &self,
        account_id: Option<&AccountId>,
        parent_hash: &CryptoHash,
        shard_id: ShardId,
        is_me: bool,
    ) -> bool {
        if let Some(account_id) = account_id {
            let account_cares_about_shard = {
                let mut epoch_manager = self.epoch_manager.write().expect(POISONED_LOCK_ERR);
                epoch_manager
                    .cares_about_shard_from_prev_block(parent_hash, account_id, shard_id)
                    .unwrap_or(false)
            };
            if !is_me {
                return account_cares_about_shard;
            }
            account_cares_about_shard || self.actual_tracked_shards.contains(&shard_id)
        } else {
            self.actual_tracked_shards.contains(&shard_id)
        }
    }

    pub fn will_care_about_shard(
        &self,
        account_id: Option<&AccountId>,
        parent_hash: &CryptoHash,
        shard_id: ShardId,
        is_me: bool,
    ) -> bool {
        if let Some(account_id) = account_id {
            let account_cares_about_shard = {
                let mut epoch_manager = self.epoch_manager.write().expect(POISONED_LOCK_ERR);
                epoch_manager
                    .cares_about_shard_next_epoch_from_prev_block(parent_hash, account_id, shard_id)
                    .unwrap_or(false)
            };
            if !is_me {
                return account_cares_about_shard;
            } else if account_cares_about_shard {
                return true;
            }
        }
        let mut tracker = self.clone();
        tracker.flush_pending(parent_hash).unwrap();
        tracker.actual_tracked_shards.contains(&shard_id)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, RwLock};

    use near_crypto::{KeyType, PublicKey};
    use near_epoch_manager::{EpochManager, RewardCalculator};
    use near_primitives::epoch_manager::{BlockInfo, EpochConfig};
    use near_primitives::hash::{hash, CryptoHash};
    use near_primitives::types::{BlockHeight, EpochId, NumShards, ValidatorStake};
    use near_store::test_utils::create_test_store;

    use super::{ShardTracker, POISONED_LOCK_ERR};
    use near_primitives::version::PROTOCOL_VERSION;
    use num_rational::Rational;

    const DEFAULT_TOTAL_SUPPLY: u128 = 1_000_000_000_000;

    fn get_epoch_manager(num_shards: NumShards) -> Arc<RwLock<EpochManager>> {
        let store = create_test_store();
        let initial_epoch_config = EpochConfig {
            epoch_length: 1,
            num_shards,
            num_block_producer_seats: 1,
            num_block_producer_seats_per_shard: vec![1],
            avg_hidden_validator_seats_per_shard: vec![],
            block_producer_kickout_threshold: 90,
            chunk_producer_kickout_threshold: 60,
            fishermen_threshold: 0,
            online_max_threshold: Rational::from_integer(1),
            online_min_threshold: Rational::new(90, 100),
            minimum_stake_divisor: 1,
            protocol_upgrade_stake_threshold: Rational::new(80, 100),
            protocol_upgrade_num_epochs: 2,
        };
        let reward_calculator = RewardCalculator {
            max_inflation_rate: Rational::from_integer(0),
            num_blocks_per_year: 1000000,
            epoch_length: 1,
            protocol_reward_percentage: Rational::from_integer(0),
            protocol_treasury_account: "".to_string(),
            online_max_threshold: initial_epoch_config.online_max_threshold,
            online_min_threshold: initial_epoch_config.online_min_threshold,
        };
        Arc::new(RwLock::new(
            EpochManager::new(
                store,
                initial_epoch_config,
                PROTOCOL_VERSION,
                reward_calculator,
                vec![ValidatorStake {
                    account_id: "test".to_string(),
                    public_key: PublicKey::empty(KeyType::ED25519),
                    stake: 100,
                }],
            )
            .unwrap(),
        ))
    }

    pub fn record_block(
        epoch_manager: &mut EpochManager,
        prev_h: CryptoHash,
        cur_h: CryptoHash,
        height: BlockHeight,
        proposals: Vec<ValidatorStake>,
    ) {
        epoch_manager
            .record_block_info(
                &cur_h,
                BlockInfo::new(
                    height,
                    0,
                    prev_h,
                    prev_h,
                    proposals,
                    vec![],
                    vec![],
                    DEFAULT_TOTAL_SUPPLY,
                    PROTOCOL_VERSION,
                ),
                [0; 32],
            )
            .unwrap()
            .commit()
            .unwrap();
    }

    #[test]
    fn test_track_new_accounts_and_shards() {
        let num_shards = 4;
        let epoch_manager = get_epoch_manager(num_shards);
        let mut tracker = ShardTracker::new(
            vec![],
            vec![],
            CryptoHash::default(),
            EpochId::default(),
            epoch_manager.clone(),
        );
        tracker.track_accounts(&["test1".to_string(), "test2".to_string()], &CryptoHash::default());
        tracker.track_shards(&[2, 3]);
        let mut epoch_manager = epoch_manager.write().expect(POISONED_LOCK_ERR);
        let mut total_tracked_shards = HashSet::new();
        total_tracked_shards.insert(
            epoch_manager
                .account_id_to_shard_id(&"test1".to_string(), &CryptoHash::default())
                .unwrap(),
        );
        total_tracked_shards.insert(
            epoch_manager
                .account_id_to_shard_id(&"test2".to_string(), &CryptoHash::default())
                .unwrap(),
        );
        total_tracked_shards.insert(2);
        total_tracked_shards.insert(3);
        assert_eq!(tracker.actual_tracked_shards, total_tracked_shards);
    }

    #[test]
    fn test_untrack_accounts() {
        let num_shards = 4;
        let epoch_manager = get_epoch_manager(num_shards);
        let mut tracker = ShardTracker::new(
            vec![],
            vec![],
            CryptoHash::default(),
            EpochId::default(),
            epoch_manager.clone(),
        );
        tracker.track_accounts(
            &["test1".to_string(), "test2".to_string(), "test3".to_string()],
            &CryptoHash::default(),
        );
        tracker.track_shards(&[2, 3]);
        {
            let mut epoch_manager = epoch_manager.write().expect(POISONED_LOCK_ERR);
            record_block(&mut epoch_manager, CryptoHash::default(), hash(&[0]), 0, vec![]);
            record_block(&mut epoch_manager, hash(&[0]), hash(&[1]), 1, vec![]);
            record_block(&mut epoch_manager, hash(&[1]), hash(&[2]), 2, vec![]);
        }
        tracker
            .untrack_accounts(&hash(&[1]), vec!["test2".to_string(), "test3".to_string()])
            .unwrap();
        tracker.update_epoch(&hash(&[2])).unwrap();

        let mut total_tracked_shards = HashSet::new();
        let mut epoch_manager = epoch_manager.write().expect(POISONED_LOCK_ERR);
        total_tracked_shards.insert(
            epoch_manager
                .account_id_to_shard_id(&"test1".to_string(), &CryptoHash::default())
                .unwrap(),
        );
        total_tracked_shards.insert(2);
        total_tracked_shards.insert(3);

        assert_eq!(tracker.actual_tracked_shards, total_tracked_shards);
    }

    #[test]
    fn test_untrack_shards() {
        let num_shards = 4;
        let epoch_manager = get_epoch_manager(num_shards);
        let mut tracker = ShardTracker::new(
            vec![],
            vec![],
            CryptoHash::default(),
            EpochId::default(),
            epoch_manager.clone(),
        );
        tracker.track_accounts(
            &["test1".to_string(), "test2".to_string(), "test3".to_string()],
            &CryptoHash::default(),
        );
        tracker.track_shards(&[2, 3]);
        {
            let mut epoch_manager = epoch_manager.write().expect(POISONED_LOCK_ERR);
            record_block(&mut epoch_manager, CryptoHash::default(), hash(&[0]), 0, vec![]);
            record_block(&mut epoch_manager, hash(&[0]), hash(&[1]), 1, vec![]);
            record_block(&mut epoch_manager, hash(&[1]), hash(&[2]), 2, vec![]);
        }
        tracker.untrack_shards(&hash(&[1]), vec![1, 2, 3]).unwrap();
        tracker.update_epoch(&hash(&[2])).unwrap();

        let mut epoch_manager = epoch_manager.write().expect(POISONED_LOCK_ERR);
        let mut total_tracked_shards = HashSet::new();
        for account_id in vec!["test1", "test2", "test3"] {
            total_tracked_shards.insert(
                epoch_manager
                    .account_id_to_shard_id(&account_id.to_string(), &CryptoHash::default())
                    .unwrap(),
            );
        }

        assert_eq!(tracker.actual_tracked_shards, total_tracked_shards);
    }
}
