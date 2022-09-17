use serde::Serialize;

use crate::{
    callbacks::Callback,
    entity::EntityID,
    game::GameBoard,
    nfts::{BaseNFT, NftPtr},
    tokens::TokenPointer,
    util::Price,
};

/// CoinLockup implements an NFT type which holds a chunk of coins and releases
/// them via a scheduled event in the future
#[derive(Serialize, Clone)]
pub(crate) struct CoinLockup {
    pub id: NftPtr,
    pub time_when_free: u64,
    pub asset: TokenPointer,
}
impl CoinLockup {
    /// Creates an NFT and transfers the requisite amount of coins to it.
    pub fn lockup(
        game: &mut GameBoard,
        owner: EntityID,
        asset: TokenPointer,
        amount: Price,
        time_when_free: u64,
    ) {
        let lockup_base = BaseNFT {
            owner,
            nft_id: game.alloc(),
            // Non Transferrable
            transfer_count: u128::max_value(),
        };
        let lockup_id = game.nfts.add(Box::new(lockup_base));
        let lockup = CoinLockup {
            time_when_free,
            asset,
            id: lockup_id,
        };
        game.callbacks.schedule(Box::new(lockup));
        game.tokens[asset].transaction();
        let _ = game.tokens[asset].transfer(&owner, &lockup_id.0, amount);
        game.tokens[asset].end_transaction();
    }
}

impl Callback for CoinLockup {
    fn time(&self) -> u64 {
        self.time_when_free
    }

    // Note: just reads immutable fields, modifies external state
    fn action(&mut self, game: &mut GameBoard) {
        let owner = game.nfts[self.id].owner();
        // this shouldn't happen if our scheduler is correct...
        if game.current_time < self.time_when_free {
            return;
        }
        let token = &mut game.tokens[self.asset];
        token.transaction();
        let balance = token.balance_check(&self.id.0);
        let _ = token.transfer(&self.id.0, &owner, balance);
        token.end_transaction();
    }

    fn purpose(&self) -> String {
        "CoinLockup Release Trigger".to_string()
    }
}
