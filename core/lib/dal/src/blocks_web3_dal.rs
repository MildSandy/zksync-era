use bigdecimal::BigDecimal;
use sqlx::Row;

use std::time::Instant;

use zksync_config::constants::EMPTY_UNCLES_HASH;
use zksync_types::{
    api,
    l2_to_l1_log::L2ToL1Log,
    vm_trace::Call,
    web3::types::{BlockHeader, U64},
    zk_evm::zkevm_opcode_defs::system_params,
    Bytes, L1BatchNumber, L2ChainId, MiniblockNumber, H160, H2048, H256, U256,
};
use zksync_utils::{bigdecimal_to_u256, miniblock_hash};

use crate::models::{
    storage_block::{bind_block_where_sql_params, web3_block_number_to_sql, web3_block_where_sql},
    storage_transaction::{extract_web3_transaction, web3_transaction_select_sql, CallTrace},
};
use crate::{SqlxError, StorageProcessor};

const BLOCK_GAS_LIMIT: u32 = system_params::VM_INITIAL_FRAME_ERGS;

#[derive(Debug)]
pub struct BlocksWeb3Dal<'a, 'c> {
    pub(crate) storage: &'a mut StorageProcessor<'c>,
}

impl BlocksWeb3Dal<'_, '_> {
    pub async fn get_sealed_miniblock_number(&mut self) -> Result<MiniblockNumber, SqlxError> {
        let started_at = Instant::now();
        let number: i64 = sqlx::query!("SELECT MAX(number) as \"number\" FROM miniblocks")
            .fetch_one(self.storage.conn())
            .await?
            .number
            .expect("DAL invocation before genesis");
        metrics::histogram!("dal.request", started_at.elapsed(), "method" => "get_sealed_block_number");
        Ok(MiniblockNumber(number as u32))
    }

    pub async fn get_sealed_l1_batch_number(&mut self) -> Result<L1BatchNumber, SqlxError> {
        let started_at = Instant::now();
        let number: i64 = sqlx::query!("SELECT MAX(number) as \"number\" FROM l1_batches")
            .fetch_one(self.storage.conn())
            .await?
            .number
            .expect("DAL invocation before genesis");
        metrics::histogram!("dal.request", started_at.elapsed(), "method" => "get_sealed_block_number");
        Ok(L1BatchNumber(number as u32))
    }

    pub async fn get_block_by_web3_block_id(
        &mut self,
        block_id: api::BlockId,
        include_full_transactions: bool,
        chain_id: L2ChainId,
    ) -> Result<Option<api::Block<api::TransactionVariant>>, SqlxError> {
        let transactions_sql = if include_full_transactions {
            web3_transaction_select_sql()
        } else {
            "transactions.hash as tx_hash"
        };

        let query = format!(
            "SELECT
                miniblocks.hash as block_hash,
                miniblocks.number,
                miniblocks.l1_batch_number,
                miniblocks.timestamp,
                miniblocks.base_fee_per_gas,
                l1_batches.timestamp as l1_batch_timestamp,
                transactions.gas_limit as gas_limit,
                transactions.refunded_gas as refunded_gas,
                {}
            FROM miniblocks
            LEFT JOIN l1_batches
                ON l1_batches.number = miniblocks.l1_batch_number
            LEFT JOIN transactions
                ON transactions.miniblock_number = miniblocks.number
            WHERE {}",
            transactions_sql,
            web3_block_where_sql(block_id, 1)
        );

        let query = bind_block_where_sql_params(&block_id, sqlx::query(&query));
        let rows = query.fetch_all(self.storage.conn()).await?.into_iter();

        let block = rows.fold(None, |prev_block, db_row| {
            let mut block = prev_block.unwrap_or_else(|| {
                // This code will be only executed for the first row in the DB response.
                // All other rows will only be used to extract relevant transactions.
                let hash = db_row
                    .try_get("block_hash")
                    .map_or_else(|_| H256::zero(), H256::from_slice);
                let number = U64::from(db_row.get::<i64, &str>("number"));
                let l1_batch_number = db_row
                    .try_get::<i64, &str>("l1_batch_number")
                    .map(U64::from)
                    .ok();
                let l1_batch_timestamp = db_row
                    .try_get::<i64, &str>("l1_batch_timestamp")
                    .map(U256::from)
                    .ok();
                let parent_hash = match number.as_u32() {
                    0 => H256::zero(),
                    number => miniblock_hash(MiniblockNumber(number - 1)),
                };
                let base_fee_per_gas = db_row.get::<BigDecimal, &str>("base_fee_per_gas");

                api::Block {
                    hash,
                    parent_hash,
                    uncles_hash: EMPTY_UNCLES_HASH,
                    number,
                    l1_batch_number,
                    gas_limit: BLOCK_GAS_LIMIT.into(),
                    base_fee_per_gas: bigdecimal_to_u256(base_fee_per_gas),
                    timestamp: db_row.get::<i64, &str>("timestamp").into(),
                    l1_batch_timestamp,
                    ..api::Block::default()
                }
            });
            if db_row.try_get::<&[u8], &str>("tx_hash").is_ok() {
                let tx_gas_limit = bigdecimal_to_u256(db_row.get::<BigDecimal, &str>("gas_limit"));
                let tx_refunded_gas = U256::from((db_row.get::<i64, &str>("refunded_gas")) as u32);

                block.gas_used += tx_gas_limit - tx_refunded_gas;
                let tx = if include_full_transactions {
                    let tx = extract_web3_transaction(db_row, chain_id);
                    api::TransactionVariant::Full(tx)
                } else {
                    api::TransactionVariant::Hash(H256::from_slice(db_row.get("tx_hash")))
                };
                block.transactions.push(tx);
            }
            Some(block)
        });
        Ok(block)
    }

    pub async fn get_block_tx_count(
        &mut self,
        block_id: api::BlockId,
    ) -> Result<Option<U256>, SqlxError> {
        let query = format!(
            "SELECT l1_tx_count + l2_tx_count as tx_count FROM miniblocks WHERE {}",
            web3_block_where_sql(block_id, 1)
        );
        let query = bind_block_where_sql_params(&block_id, sqlx::query(&query));

        let tx_count: Option<i32> = query
            .fetch_optional(self.storage.conn())
            .await?
            .map(|db_row| db_row.get("tx_count"));
        Ok(tx_count.map(|t| (t as u32).into()))
    }

    /// Returns hashes of blocks with numbers greater than `from_block` and the number of the last block.
    pub async fn get_block_hashes_after(
        &mut self,
        from_block: MiniblockNumber,
        limit: usize,
    ) -> Result<(Vec<H256>, Option<MiniblockNumber>), SqlxError> {
        let rows = sqlx::query!(
            "SELECT number, hash FROM miniblocks \
            WHERE number > $1 \
            ORDER BY number ASC \
            LIMIT $2",
            from_block.0 as i64,
            limit as i32
        )
        .fetch_all(self.storage.conn())
        .await?;

        let last_block_number = rows.last().map(|row| MiniblockNumber(row.number as u32));
        let hashes = rows.iter().map(|row| H256::from_slice(&row.hash)).collect();
        Ok((hashes, last_block_number))
    }

    /// Returns hashes of blocks with numbers greater than `from_block` and the number of the last block.
    pub async fn get_block_headers_after(
        &mut self,
        from_block: MiniblockNumber,
    ) -> Result<Vec<BlockHeader>, SqlxError> {
        let rows = sqlx::query!(
            "SELECT hash, number, timestamp \
            FROM miniblocks \
            WHERE number > $1 \
            ORDER BY number ASC",
            from_block.0 as i64,
        )
        .fetch_all(self.storage.conn())
        .await?;

        let blocks = rows.into_iter().map(|row| BlockHeader {
            hash: Some(H256::from_slice(&row.hash)),
            parent_hash: H256::zero(),
            uncles_hash: EMPTY_UNCLES_HASH,
            author: H160::zero(),
            state_root: H256::zero(),
            transactions_root: H256::zero(),
            receipts_root: H256::zero(),
            number: Some(U64::from(row.number)),
            gas_used: U256::zero(),
            gas_limit: U256::zero(),
            base_fee_per_gas: None,
            extra_data: Bytes::default(),
            logs_bloom: H2048::default(),
            timestamp: U256::from(row.timestamp),
            difficulty: U256::zero(),
            mix_hash: None,
            nonce: None,
        });
        Ok(blocks.collect())
    }

    pub async fn resolve_block_id(
        &mut self,
        block_id: api::BlockId,
    ) -> Result<Option<MiniblockNumber>, SqlxError> {
        let query_string = match block_id {
            api::BlockId::Hash(_) => "SELECT number FROM miniblocks WHERE hash = $1".to_owned(),
            api::BlockId::Number(api::BlockNumber::Number(_)) => {
                // The reason why instead of returning the `block_number` directly we use query is
                // to handle numbers of blocks that are not created yet.
                // the `SELECT number FROM miniblocks WHERE number=block_number` for
                // non-existing block number will returns zero.
                "SELECT number FROM miniblocks WHERE number = $1".to_owned()
            }
            api::BlockId::Number(api::BlockNumber::Earliest) => {
                return Ok(Some(MiniblockNumber(0)));
            }
            api::BlockId::Number(block_number) => web3_block_number_to_sql(block_number),
        };
        let row = bind_block_where_sql_params(&block_id, sqlx::query(&query_string))
            .fetch_optional(self.storage.conn())
            .await?;

        let block_number = row
            .and_then(|row| row.get::<Option<i64>, &str>("number"))
            .map(|n| MiniblockNumber(n as u32));
        Ok(block_number)
    }

    pub async fn get_block_timestamp(
        &mut self,
        block_number: MiniblockNumber,
    ) -> Result<Option<u64>, SqlxError> {
        let timestamp = sqlx::query!(
            "SELECT timestamp FROM miniblocks WHERE number = $1",
            block_number.0 as i64
        )
        .fetch_optional(self.storage.conn())
        .await?
        .map(|row| row.timestamp as u64);
        Ok(timestamp)
    }

    pub async fn get_l2_to_l1_logs(
        &mut self,
        block_number: L1BatchNumber,
    ) -> Result<Vec<L2ToL1Log>, SqlxError> {
        let raw_logs = sqlx::query!(
            "SELECT l2_to_l1_logs FROM l1_batches WHERE number = $1",
            block_number.0 as i64
        )
        .fetch_optional(self.storage.conn())
        .await?
        .map(|row| row.l2_to_l1_logs)
        .unwrap_or_default();

        Ok(raw_logs
            .into_iter()
            .map(|bytes| L2ToL1Log::from_slice(&bytes))
            .collect())
    }

    pub async fn get_l1_batch_number_of_miniblock(
        &mut self,
        miniblock_number: MiniblockNumber,
    ) -> Result<Option<L1BatchNumber>, SqlxError> {
        let number: Option<i64> = sqlx::query!(
            "SELECT l1_batch_number FROM miniblocks WHERE number = $1",
            miniblock_number.0 as i64
        )
        .fetch_optional(self.storage.conn())
        .await?
        .and_then(|row| row.l1_batch_number);

        Ok(number.map(|number| L1BatchNumber(number as u32)))
    }

    pub async fn get_miniblock_range_of_l1_batch(
        &mut self,
        l1_batch_number: L1BatchNumber,
    ) -> Result<Option<(MiniblockNumber, MiniblockNumber)>, SqlxError> {
        let row = sqlx::query!(
            "SELECT MIN(miniblocks.number) as \"min?\", MAX(miniblocks.number) as \"max?\" \
            FROM miniblocks \
            WHERE l1_batch_number = $1",
            l1_batch_number.0 as i64
        )
        .fetch_one(self.storage.conn())
        .await?;

        Ok(match (row.min, row.max) {
            (Some(min), Some(max)) => {
                Some((MiniblockNumber(min as u32), MiniblockNumber(max as u32)))
            }
            (None, None) => None,
            _ => unreachable!(),
        })
    }

    pub async fn get_l1_batch_info_for_tx(
        &mut self,
        tx_hash: H256,
    ) -> Result<Option<(L1BatchNumber, u16)>, SqlxError> {
        let row = sqlx::query!(
            "SELECT l1_batch_number, l1_batch_tx_index \
            FROM transactions \
            WHERE hash = $1",
            tx_hash.as_bytes()
        )
        .fetch_optional(self.storage.conn())
        .await?;

        let result = row.and_then(|row| match (row.l1_batch_number, row.l1_batch_tx_index) {
            (Some(l1_batch_number), Some(l1_batch_tx_index)) => Some((
                L1BatchNumber(l1_batch_number as u32),
                l1_batch_tx_index as u16,
            )),
            _ => None,
        });
        Ok(result)
    }

    pub async fn get_trace_for_miniblock(&mut self, block_number: MiniblockNumber) -> Vec<Call> {
        sqlx::query_as!(
            CallTrace,
            "SELECT * FROM call_traces WHERE tx_hash IN \
                (SELECT hash FROM transactions WHERE miniblock_number = $1)",
            block_number.0 as i64
        )
        .fetch_all(self.storage.conn())
        .await
        .unwrap()
        .into_iter()
        .map(Call::from)
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use db_test_macro::db_test;
    use zksync_types::{block::MiniblockHeader, MiniblockNumber};

    use super::*;
    use crate::{tests::create_miniblock_header, ConnectionPool};

    #[db_test(dal_crate)]
    async fn getting_web3_block_and_tx_count(connection_pool: ConnectionPool) {
        let mut conn = connection_pool.access_test_storage().await;
        conn.blocks_dal()
            .delete_miniblocks(MiniblockNumber(0))
            .await;
        let header = MiniblockHeader {
            l1_tx_count: 3,
            l2_tx_count: 5,
            ..create_miniblock_header(0)
        };
        conn.blocks_dal().insert_miniblock(&header).await;

        let block_ids = [
            api::BlockId::Number(api::BlockNumber::Earliest),
            api::BlockId::Number(api::BlockNumber::Latest),
            api::BlockId::Number(api::BlockNumber::Number(0.into())),
            api::BlockId::Hash(miniblock_hash(MiniblockNumber(0))),
        ];
        for block_id in block_ids {
            let block = conn
                .blocks_web3_dal()
                .get_block_by_web3_block_id(block_id, false, L2ChainId(270))
                .await;
            let block = block.unwrap().unwrap();
            assert!(block.transactions.is_empty());
            assert_eq!(block.number, U64::zero());
            assert_eq!(block.hash, miniblock_hash(MiniblockNumber(0)));

            let tx_count = conn.blocks_web3_dal().get_block_tx_count(block_id).await;
            assert_eq!(tx_count.unwrap(), Some(8.into()));
        }

        let non_existing_block_ids = [
            api::BlockId::Number(api::BlockNumber::Pending),
            api::BlockId::Number(api::BlockNumber::Number(1.into())),
            api::BlockId::Hash(miniblock_hash(MiniblockNumber(1))),
        ];
        for block_id in non_existing_block_ids {
            let block = conn
                .blocks_web3_dal()
                .get_block_by_web3_block_id(block_id, false, L2ChainId(270))
                .await;
            assert!(block.unwrap().is_none());

            let tx_count = conn.blocks_web3_dal().get_block_tx_count(block_id).await;
            assert_eq!(tx_count.unwrap(), None);
        }
    }

    #[db_test(dal_crate)]
    async fn resolving_earliest_block_id(connection_pool: ConnectionPool) {
        let mut conn = connection_pool.access_test_storage().await;
        conn.blocks_dal()
            .delete_miniblocks(MiniblockNumber(0))
            .await;

        let miniblock_number = conn
            .blocks_web3_dal()
            .resolve_block_id(api::BlockId::Number(api::BlockNumber::Earliest))
            .await;
        assert_eq!(miniblock_number.unwrap(), Some(MiniblockNumber(0)));
    }

    #[db_test(dal_crate)]
    async fn resolving_latest_block_id(connection_pool: ConnectionPool) {
        let mut conn = connection_pool.access_test_storage().await;
        conn.blocks_dal()
            .delete_miniblocks(MiniblockNumber(0))
            .await;
        conn.blocks_dal()
            .insert_miniblock(&create_miniblock_header(0))
            .await;

        let miniblock_number = conn
            .blocks_web3_dal()
            .resolve_block_id(api::BlockId::Number(api::BlockNumber::Latest))
            .await;
        assert_eq!(miniblock_number.unwrap(), Some(MiniblockNumber(0)));

        let miniblock_number = conn
            .blocks_web3_dal()
            .resolve_block_id(api::BlockId::Number(api::BlockNumber::Number(0.into())))
            .await;
        assert_eq!(miniblock_number.unwrap(), Some(MiniblockNumber(0)));
        let miniblock_number = conn
            .blocks_web3_dal()
            .resolve_block_id(api::BlockId::Number(api::BlockNumber::Number(1.into())))
            .await;
        assert_eq!(miniblock_number.unwrap(), None);

        conn.blocks_dal()
            .insert_miniblock(&create_miniblock_header(1))
            .await;
        let miniblock_number = conn
            .blocks_web3_dal()
            .resolve_block_id(api::BlockId::Number(api::BlockNumber::Latest))
            .await;
        assert_eq!(miniblock_number.unwrap(), Some(MiniblockNumber(1)));

        let miniblock_number = conn
            .blocks_web3_dal()
            .resolve_block_id(api::BlockId::Number(api::BlockNumber::Pending))
            .await;
        assert_eq!(miniblock_number.unwrap(), Some(MiniblockNumber(2)));

        let miniblock_number = conn
            .blocks_web3_dal()
            .resolve_block_id(api::BlockId::Number(api::BlockNumber::Number(1.into())))
            .await;
        assert_eq!(miniblock_number.unwrap(), Some(MiniblockNumber(1)));
    }

    #[db_test(dal_crate)]
    async fn resolving_block_by_hash(connection_pool: ConnectionPool) {
        let mut conn = connection_pool.access_test_storage().await;
        conn.blocks_dal()
            .delete_miniblocks(MiniblockNumber(0))
            .await;
        conn.blocks_dal()
            .insert_miniblock(&create_miniblock_header(0))
            .await;

        let hash = miniblock_hash(MiniblockNumber(0));
        let miniblock_number = conn
            .blocks_web3_dal()
            .resolve_block_id(api::BlockId::Hash(hash))
            .await;
        assert_eq!(miniblock_number.unwrap(), Some(MiniblockNumber(0)));

        let hash = miniblock_hash(MiniblockNumber(1));
        let miniblock_number = conn
            .blocks_web3_dal()
            .resolve_block_id(api::BlockId::Hash(hash))
            .await;
        assert_eq!(miniblock_number.unwrap(), None);
    }
}
