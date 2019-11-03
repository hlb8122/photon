use bitcoin::{
    consensus::encode::Error as ConsensusError,
    consensus::encode::{Decodable, Encodable},
    Block,
};
use futures::{future::join_all, prelude::*};
use rocksdb::Error as RocksError;

use super::{client::*, tx_processing::*};
use crate::db::Database;

const BLOCK_CHUNK_SIZE: usize = 128;

#[derive(Debug)]
pub enum BlockProcessingError {
    BlockDecode(BitcoinError),
    BlockDecoding(ConsensusError),
    Transaction(TxProcessingError),
    Database(RocksError),
}

impl From<TxProcessingError> for BlockProcessingError {
    fn from(err: TxProcessingError) -> Self {
        BlockProcessingError::Transaction(err)
    }
}

impl From<ConsensusError> for BlockProcessingError {
    fn from(err: ConsensusError) -> Self {
        BlockProcessingError::BlockDecoding(err)
    }
}

impl From<RocksError> for BlockProcessingError {
    fn from(err: RocksError) -> Self {
        BlockProcessingError::Database(err)
    }
}

pub async fn par_process_block_stream(
    raw_block_stream: impl Stream<Item = Result<(u32, Vec<u8>), BitcoinError>> + Send,
    db: Database,
    block_callback: &dyn Fn(u32) -> Result<(), BlockProcessingError>,
) -> Result<(), BlockProcessingError> {
    // Split stream into chunks
    let block_stream = raw_block_stream.chunks(BLOCK_CHUNK_SIZE).map(
        // Convert Vec<Result<_, _> into Result<Vec<_>, _>
        // TODO: This could very well be a bottleneck here
        // Reevaluate this later
        move |result_vector: Vec<Result<(u32, Vec<u8>), BitcoinError>>| {
            result_vector
                .into_iter()
                .fold(Ok(vec![]), move |mut output_res, result| {
                    let (height, raw_block) = match result {
                        Ok(ok) => ok,
                        Err(err) => {
                            warn!("failed to fetch block {:?}", err);
                            return Err(BlockProcessingError::BlockDecode(err));
                        }
                    };
                    let block = Block::consensus_decode(&raw_block[..])?;
                    output_res
                        .as_mut()
                        .map(|output| output.push((height, block)));
                    output_res
                })
        },
    );

    // TODO: Reevaluate this later
    let processing =
        block_stream.try_for_each_concurrent(256, move |res_vec: Vec<(u32, Block)>| {
            let db_inner = db.clone();
            let chunked_iter =
                res_vec
                    .into_iter()
                    .map(move |(block_height, block): (u32, Block)| {
                        let db_inner = db_inner.clone();
                        async move {
                            // Process header
                            let mut raw_header: [u8; 80] = [0; 80];
                            block.header.consensus_encode(&mut raw_header[..]).unwrap();
                            db_inner.put_header(block_height, &raw_header)?;

                            // Do some action dependending on block height
                            block_callback(block_height)?;

                            // Process transactions
                            let txs = block.txdata;
                            Ok(process_transactions(block_height, txs, db_inner.clone()).await?)
                        }
                    });
            join_all(chunked_iter).map(|result| {
                result
                    .into_iter()
                    .collect::<Result<_, BlockProcessingError>>()
            })
        });
    processing.await
}
