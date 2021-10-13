use crate::array::DataChunkRef;
use crate::buffer::Bitmap;
use crate::error::ErrorCode::InternalError;
use crate::error::{ErrorCode, Result};
use crate::task::channel::{BoxChanReceiver, BoxChanSender, ChanReceiver, ChanSender};
use crate::util::hash_util::CRC32FastBuilder;
use risingwave_proto::plan::*;
use std::option::Option;
use std::sync::mpsc;
use std::sync::Arc;

pub struct HashShuffleSender {
    senders: Vec<mpsc::Sender<DataChunkRef>>,
    hash_info: ShuffleInfo_HashInfo,
}

pub struct HashShuffleReceiver {
    receiver: mpsc::Receiver<DataChunkRef>,
}

fn generate_hash_values(
    chunk: DataChunkRef,
    hash_info: &ShuffleInfo_HashInfo,
) -> Result<Vec<usize>> {
    let output_count = hash_info.output_count as usize;

    let hasher_builder = match hash_info.hash_method {
        ShuffleInfo_HashInfo_HashMethod::CRC32 => CRC32FastBuilder {},
    };

    let hash_values = chunk
        .get_hash_values(
            &hash_info
                .keys
                .iter()
                .map(|key| *key as usize)
                .collect::<Vec<_>>(),
            hasher_builder,
        )
        .map_err(|e| InternalError(format!("get_hash_values:{}", e)))?
        .iter_mut()
        .map(|hash_value| *hash_value as usize % output_count)
        .collect::<Vec<_>>();
    Ok(hash_values)
}

fn generate_new_data_chunks(
    chunk: DataChunkRef,
    hash_info: &ShuffleInfo_HashInfo,
    hash_values: &[usize],
) -> Result<Vec<DataChunkRef>> {
    let output_count = hash_info.output_count as usize;
    let mut vis_maps = vec![vec![]; output_count];
    hash_values.iter().for_each(|hash| {
        for (sink_id, vis_map) in vis_maps.iter_mut().enumerate() {
            if *hash == sink_id {
                vis_map.push(true);
            } else {
                vis_map.push(false);
            }
        }
    });
    let mut res = Vec::with_capacity(output_count);
    for (sink_id, vis_map_vec) in vis_maps.into_iter().enumerate() {
        let vis_map = Bitmap::from_vec(vis_map_vec)?;
        let new_data_chunk = chunk.with_visibility(vis_map).compact()?;
        debug!(
            "send to sink:{}, cardinality:{}",
            sink_id,
            new_data_chunk.cardinality()
        );
        res.push(Arc::new(new_data_chunk));
    }
    Ok(res)
}

#[async_trait::async_trait]
impl ChanSender for HashShuffleSender {
    async fn send(&mut self, chunk: DataChunkRef) -> Result<()> {
        let hash_values = generate_hash_values(chunk.clone(), &self.hash_info)?;
        let new_data_chunks = generate_new_data_chunks(chunk, &self.hash_info, &hash_values)?;

        for (sink_id, new_data_chunk) in new_data_chunks.into_iter().enumerate() {
            debug!(
                "send to sink:{}, cardinality:{}",
                sink_id,
                new_data_chunk.cardinality()
            );
            let res: Result<()> = self.senders[sink_id].send(new_data_chunk).map_err(|e| {
                ErrorCode::InternalError(format!("chunk was sent to a closed channel {}", e)).into()
            });
            res?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ChanReceiver for HashShuffleReceiver {
    async fn recv(&mut self) -> Option<DataChunkRef> {
        match self.receiver.recv() {
            Err(_) => None, // Sender is dropped.
            Ok(chunk) => Some(chunk),
        }
    }
}

pub fn new_hash_shuffle_channel(shuffle: &ShuffleInfo) -> (BoxChanSender, Vec<BoxChanReceiver>) {
    let hash_info = shuffle.get_hash_info();
    let output_count = hash_info.output_count as usize;
    let mut senders = Vec::with_capacity(output_count);
    let mut receivers = Vec::with_capacity(output_count);
    for _ in 0..output_count {
        let (s, r) = mpsc::channel();
        senders.push(s);
        receivers.push(r);
    }
    let channel_sender = Box::new(HashShuffleSender {
        senders,
        hash_info: hash_info.clone(),
    }) as BoxChanSender;
    let channel_receivers = receivers
        .into_iter()
        .map(|receiver| Box::new(HashShuffleReceiver { receiver }) as BoxChanReceiver)
        .collect::<Vec<_>>();
    (channel_sender, channel_receivers)
}

#[cfg(test)]
mod tests {
    use crate::risingwave_proto::plan::*;
    use crate::task::test_utils::{ResultChecker, TestRunner};
    use crate::util::hash_util::CRC32FastBuilder;
    use rand::Rng;
    use std::hash::BuildHasher;

    pub fn hash_shuffle_plan(plan: &mut PlanFragment, keys: Vec<u32>, num_sinks: u32) {
        let mut hash_info = ShuffleInfo_HashInfo::default();
        hash_info.set_hash_method(ShuffleInfo_HashInfo_HashMethod::CRC32);
        hash_info.set_keys(keys);
        hash_info.set_output_count(num_sinks);
        let shuffle_info_oneof_shuffle_info =
            ShuffleInfo_oneof_shuffle_info::hash_info(hash_info.clone());
        let mut shuffle_info = ShuffleInfo::default();
        shuffle_info.set_hash_info(hash_info);
        shuffle_info.shuffle_info = Some(shuffle_info_oneof_shuffle_info);
        shuffle_info.partition_mode = ShuffleInfo_PartitionMode::HASH;
        plan.set_shuffle_info(shuffle_info);
    }

    #[test]
    fn test_hash_shuffle() {
        let test_case = |num_columns: usize, num_rows: usize, num_sinks: u32, keys: Vec<u32>| {
            let mut rng = rand::thread_rng();
            let mut rows = vec![];
            for _row_idx in 0..num_rows {
                let mut row = vec![];
                for _col_idx in 0..num_columns {
                    row.push(rng.gen::<i32>());
                }
                rows.push(row);
            }

            let mut runner = TestRunner::new();
            let mut table_builder = runner.prepare_table().create_table_int32s(num_columns);
            for row in &rows {
                table_builder = table_builder.insert_i32s(row);
            }
            table_builder.run();

            let mut builder = runner.prepare_scan().scan_all();
            let hashes = rows
                .iter()
                .map(|row| {
                    let hash_builder = CRC32FastBuilder {};
                    let mut hasher = hash_builder.build_hasher();
                    keys.iter().for_each(|key| {
                        let bs = row[*key as usize].to_le_bytes();
                        hasher.update(&bs);
                    });
                    hasher.finalize() % num_sinks
                })
                .collect::<Vec<u32>>();
            let mut each_sink_output_columns = vec![vec![vec![]; num_columns]; num_sinks as usize];
            hashes.iter().zip(rows.iter()).for_each(|(hash, row)| {
                let output_columns = &mut each_sink_output_columns[*hash as usize];
                for (col_idx, num) in row.iter().enumerate() {
                    output_columns[col_idx].push(*num);
                }
            });
            hash_shuffle_plan(builder.get_mut_plan(), keys, num_sinks);
            let res = builder.run_and_collect_multiple_output();
            assert_eq!(num_sinks as usize, res.len());
            for (sink_id, col) in res.into_iter().enumerate() {
                let mut res_checker = ResultChecker::new();
                for column in each_sink_output_columns[sink_id].iter() {
                    res_checker.add_i32_column(false, column.as_slice());
                }
                res_checker.check_result(&col);
            }
        };

        test_case(1, 1, 3, vec![0]);
        test_case(2, 2, 5, vec![0]);
        test_case(10, 10, 5, vec![0, 3, 5]);
        test_case(100, 100, 7, vec![0, 2, 51, 98]);
    }
}
