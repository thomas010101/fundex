mod event;
mod online;
mod reference;

pub use event::ProofOfIndexingEvent;
pub use online::{BlockEventStream, ProofOfIndexing, ProofOfIndexingFinisher};

use atomic_refcell::AtomicRefCell;
use std::sync::Arc;

pub type SharedProofOfIndexing = Option<Arc<AtomicRefCell<ProofOfIndexing>>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::{EthereumBlockPointer, SubgraphDeploymentId, Value};
    use maplit::hashmap;
    use online::ProofOfIndexingFinisher;
    use reference::*;
    use slog::{o, Discard, Logger};
    use stable_hash::crypto::SetHasher;
    use stable_hash::utils::stable_hash;
    use std::collections::HashMap;
    use std::convert::TryInto;
    use web3::types::{Address, H256};

    fn check_equal(reference: &PoI) -> String {
        let logger = Logger::root(Discard, o!());

        let mut db = HashMap::<String, Vec<u8>>::new();

        let mut block_count = 1;
        for causality_region in reference.causality_regions.values() {
            block_count = causality_region.blocks.len();
            break;
        }

        for block_i in 0..block_count {
            let mut stream = ProofOfIndexing::new(block_i.try_into().unwrap());

            for (name, region) in reference.causality_regions.iter() {
                let block = &region.blocks[block_i];

                for evt in block.events.iter() {
                    stream.write(&logger, name, evt);
                }
            }

            for (name, region) in stream.take() {
                let prev = db.get(&name);
                let update = region.pause(prev.map(|v| &v[..]));
                db.insert(name, update);
            }
        }

        let block_number = (block_count - 1) as u64;
        let block_ptr = EthereumBlockPointer::from((reference.block_hash, block_number));

        // This region emulates the request
        let mut finisher =
            ProofOfIndexingFinisher::new(&block_ptr, &reference.subgraph_id, &reference.indexer);
        for (name, region) in db.iter() {
            finisher.add_causality_region(name, region);
        }

        let online = hex::encode(finisher.finish());
        let offline = hex::encode(stable_hash::<SetHasher, _>(reference));
        assert_eq!(&online, &offline);
        offline
    }

    /// This test checks that each case resolves to a unique hash, and that
    /// in each case the reference and online versions match
    #[test]
    fn online_vs_reference() {
        let data = hashmap! {
            "val".to_owned() => Value::Int(1)
        };
        let data_empty = hashmap! {};
        let data2 = hashmap! {
            "key".to_owned() => Value::String("s".to_owned()),
            "null".to_owned() => Value::Null,
        };

        let mut cases = hashmap! {
            // Simple case of basically nothing
            "genesis" => PoI {
                subgraph_id: SubgraphDeploymentId::new("test").unwrap(),
                block_hash: H256::repeat_byte(1),
                causality_regions: HashMap::new(),
                indexer: None,
            },

            // Add an event
            "one_event" => PoI {
                subgraph_id: SubgraphDeploymentId::new("test").unwrap(),
                block_hash: H256::repeat_byte(1),
                causality_regions: hashmap! {
                    "eth".to_owned() => CausalityRegion {
                        blocks: vec! [
                            Block::default(),
                            Block {
                                events: vec![
                                    ProofOfIndexingEvent::SetEntity {
                                        entity_type: "t",
                                        id: "id",
                                        data: &data_empty,
                                    }
                                ]
                            }
                        ],
                    },
                },
                indexer: Some(Address::repeat_byte(1)),
            },

            // Try adding a couple more blocks, including an empty block on the end
            "multiple_blocks" => PoI {
                subgraph_id: SubgraphDeploymentId::new("b").unwrap(),
                block_hash: H256::repeat_byte(3),
                causality_regions: hashmap! {
                    "eth".to_owned() => CausalityRegion {
                        blocks: vec! [
                            Block::default(),
                            Block {
                                events: vec![
                                    ProofOfIndexingEvent::SetEntity {
                                        entity_type: "type",
                                        id: "id",
                                        data: &data,
                                    }
                                ]
                            },
                            Block::default(),
                            Block {
                                events: vec![
                                    ProofOfIndexingEvent::SetEntity {
                                        entity_type: "type",
                                        id: "id",
                                        data: &data_empty,
                                    }
                                ]
                            },
                            Block::default(),
                        ],
                    },
                },
                indexer: Some(Address::repeat_byte(1)),
            },

            // Try adding another causality region
            "causality_regions" => PoI {
                subgraph_id: SubgraphDeploymentId::new("b").unwrap(),
                block_hash: H256::repeat_byte(3),
                causality_regions: hashmap! {
                    "eth".to_owned() => CausalityRegion {
                        blocks: vec! [
                            Block::default(),
                            Block {
                                events: vec![
                                    ProofOfIndexingEvent::SetEntity {
                                        entity_type: "type",
                                        id: "id",
                                        data: &data2,
                                    }
                                ]
                            },
                            Block::default(),
                            Block {
                                events: vec![
                                    ProofOfIndexingEvent::RemoveEntity {
                                        entity_type: "type",
                                        id: "id",
                                    }
                                ]
                            },
                            Block::default(),
                        ],
                    },
                    "ipfs".to_owned() => CausalityRegion {
                        blocks: vec! [
                            Block::default(),
                            Block {
                                events: vec![
                                    ProofOfIndexingEvent::SetEntity {
                                        entity_type: "type",
                                        id: "id",
                                        data: &data,
                                    }
                                ]
                            },
                            Block::default(),
                            Block {
                                events: vec![
                                    ProofOfIndexingEvent::SetEntity {
                                        entity_type: "type",
                                        id: "id",
                                        data: &data,
                                    }
                                ]
                            },
                            Block::default(),
                        ],
                    },
                },
                indexer: Some(Address::repeat_byte(1)),
            },

            // Back to the one event case, but try adding some data.
            "data" => PoI {
                subgraph_id: SubgraphDeploymentId::new("test").unwrap(),
                block_hash: H256::repeat_byte(1),
                causality_regions: hashmap! {
                    "eth".to_owned() => CausalityRegion {
                        blocks: vec! [
                            Block::default(),
                            Block {
                                events: vec![
                                    ProofOfIndexingEvent::SetEntity {
                                        entity_type: "type",
                                        id: "id",
                                        data: &data,
                                    }
                                ]
                            }
                        ],
                    },
                },
                indexer: Some(Address::repeat_byte(4)),
            },
        };

        // Lots of data up there ⬆️ to test. Finally, loop over each case, comparing the reference and
        // online version, then checking that there are no conflicts for the reference versions.
        let mut results = HashMap::new();
        for (name, data) in cases.drain() {
            let result = check_equal(&data);
            if let Some(prev) = results.insert(result, name) {
                assert!(false, "Found conflict for case: {} == {}", name, prev);
            }
        }
    }
}
