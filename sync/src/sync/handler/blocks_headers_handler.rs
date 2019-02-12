/*******************************************************************************
 * Copyright (c) 2018-2019 Aion foundation.
 *
 *     This file is part of the aion network project.
 *
 *     The aion network project is free software: you can redistribute it
 *     and/or modify it under the terms of the GNU General Public License
 *     as published by the Free Software Foundation, either version 3 of
 *     the License, or any later version.
 *
 *     The aion network project is distributed in the hope that it will
 *     be useful, but WITHOUT ANY WARRANTY; without even the implied
 *     warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
 *     See the GNU General Public License for more details.
 *
 *     You should have received a copy of the GNU General Public License
 *     along with the aion network project source files.
 *     If not, see <https://www.gnu.org/licenses/>.
 *
 ******************************************************************************/

use acore::engines::pow_equihash_engine::POWEquihashEngine;
use acore::header::Header as BlockHeader;
use acore::client::BlockStatus;
use acore_bytes::to_hex;
use byteorder::{BigEndian, ByteOrder};
use bytes::BufMut;
use rlp::UntrustedRlp;
use std::time::{Duration, SystemTime};
use kvdb::DBTransaction;

use super::super::action::SyncAction;
use super::super::event::SyncEvent;
use super::super::storage::{BlockWrapper, HeadersWrapper, SyncStorage, MAX_CACHED_BLOCK_HASHED};
use super::blocks_bodies_handler::BlockBodiesHandler;

use p2p::*;

const BACKWARD_SYNC_STEP: u64 = 128;
const REQUEST_SIZE: u64 = 96;

pub struct BlockHeadersHandler;

impl BlockHeadersHandler {
    pub fn get_headers_from_node(node: &mut Node) {
        trace!(target: "sync", "get_headers_from_node, node id: {}", node.get_node_id());

        if P2pMgr::get_network_config().sync_from_boot_nodes_only && !node.is_from_boot_list {
            return;
        }

        if node.last_request_timestamp + Duration::from_secs(1) > SystemTime::now() {
            return;
        }

        if node.synced_block_num == 0 {
            node.synced_block_num = SyncStorage::get_synced_block_number() + 1;
        }

        if SyncStorage::get_synced_block_number() + ((MAX_CACHED_BLOCK_HASHED / 4) as u64)
            <= node.synced_block_num
        {
            debug!(target: "sync", "get_headers_from_node, {} - {}", SyncStorage::get_synced_block_number(), node.synced_block_num);

            return;
        }

        if node.target_total_difficulty > node.current_total_difficulty {
            let mut from: u64 = 1;
            let size = REQUEST_SIZE;

            match node.mode {
                Mode::NORMAL => {
                    if node.synced_block_num + 128 < SyncStorage::get_synced_block_number() {
                        node.synced_block_num = SyncStorage::get_synced_block_number();
                    }

                    let self_num = node.synced_block_num;
                    from = if self_num > 2 { self_num - 1 } else { 1 };
                }
                Mode::BACKWARD => {
                    let self_num = node.synced_block_num;
                    if self_num > BACKWARD_SYNC_STEP {
                        from = self_num - BACKWARD_SYNC_STEP;
                    }
                }
                Mode::FORWARD => {
                    let self_num = node.synced_block_num;
                    from = self_num + 1;
                }
            };

            if node.last_request_num == from {
                return;
            } else {
                node.last_request_timestamp = SystemTime::now();
            }
            node.last_request_num = from;

            debug!(target: "sync", "request headers: from number: {}, node: {}, sn: {}, mode: {}.", from, node.get_ip_addr(), node.synced_block_num, node.mode);

            Self::send_blocks_headers_req(node.node_hash, from, size as u32);
            P2pMgr::update_node(node.node_hash, node);
        }
    }

    fn send_blocks_headers_req(node_hash: u64, from: u64, size: u32) {
        let mut req = ChannelBuffer::new();
        req.head.ver = Version::V0.value();
        req.head.ctrl = Control::SYNC.value();
        req.head.action = SyncAction::BLOCKSHEADERSREQ.value();

        let mut from_buf = [0; 8];
        BigEndian::write_u64(&mut from_buf, from);
        req.body.put_slice(&from_buf);

        let mut size_buf = [0; 4];
        BigEndian::write_u32(&mut size_buf, size);
        req.body.put_slice(&size_buf);

        req.head.len = req.body.len() as u32;

        P2pMgr::send(node_hash, req);
    }

    pub fn handle_blocks_headers_req(_node: &mut Node, _req: ChannelBuffer) {
        trace!(target: "sync", "BLOCKSHEADERSREQ received.");
    }

    pub fn handle_blocks_headers_res(node: &mut Node, req: ChannelBuffer) {
        trace!(target: "sync", "BLOCKSHEADERSRES received.");

        if node.target_total_difficulty < SyncStorage::get_network_total_diff() {
            info!(target: "sync", "target_total_difficulty: {}, network_total_diff: {}.", node.target_total_difficulty, SyncStorage::get_network_total_diff());
            // return;
        }

        let node_hash = node.node_hash;
        let rlp = UntrustedRlp::new(req.body.as_slice());
        let mut prev_header = BlockHeader::new();
        let mut headers = Vec::new();

        for header_rlp in rlp.iter() {
            if let Ok(header) = header_rlp.as_val() {
                let result = POWEquihashEngine::validate_block_header(&header);
                match result {
                    Ok(()) => {
                        // break if not consisting
                        if prev_header.number() != 0
                            && (header.number() != prev_header.number() + 1
                                || prev_header.hash() != *header.parent_hash())
                        {
                            error!(target: "sync",
                            "<inconsistent-block-headers num={}, prev+1={}, hash={}, p_hash={}>, hash={}>",
                            header.number(),
                            prev_header.number() + 1,
                            header.parent_hash(),
                            prev_header.hash(),
                            header.hash(),
                        );
                            break;
                        } else {
                            let hash = header.hash();
                            let number = header.number();

                            if number <= SyncStorage::get_synced_block_number() {
                                debug!(target: "sync", "Imported header: {} - {:?}.", number, hash);
                            } else if SyncStorage::is_block_hash_confirmed(hash, true) {
                                headers.push(header.clone());
                                debug!(target: "sync", "Confirmed header: {} - {:?}, to be imported.", number, hash);
                            } else {
                                debug!(target: "sync", "Downloaded header: {} - {:?}, under confirmation.", number, hash);
                            }
                            node.synced_block_num = number;
                        }
                        prev_header = header;
                    }
                    Err(e) => {
                        // ignore this batch if any invalidated header
                        error!(target: "sync", "Invalid header: {:?}, header: {}, received from {}@{}", e, to_hex(header_rlp.as_raw()), node.get_node_id(), node.get_ip_addr());
                    }
                }
            } else {
                error!(target: "sync", "Invalid header: {}, received from {}@{}", to_hex(header_rlp.as_raw()), node.get_node_id(), node.get_ip_addr());
            }
        }

        if !headers.is_empty() {
            node.inc_reputation(10);
            Self::import_block_header(node_hash, headers);
            Self::get_headers_from_node(node);
        } else {
            node.inc_reputation(1);
            debug!(target: "sync", "Came too late............");
        }

        SyncEvent::update_node_state(node, SyncEvent::OnBlockHeadersRes);
        P2pMgr::update_node(node_hash, node);
    }

    fn import_block_header(node_hash: u64, headers: Vec<BlockHeader>) {
        let mut count = 0;
        let mut hw = HeadersWrapper::new();
        let mut local_status = SyncStorage::get_local_status();
        for header in headers.iter() {
			let mut tx = DBTransaction::new();
            let mut header_chain = SyncStorage::get_block_header_chain();
            if header_chain.status(header.parent_hash()) != BlockStatus::InChain {
                break;
            }
			if let Ok(pending) = header_chain.insert(&mut tx, &header, None) {
                header_chain.apply_pending(tx, pending);
			}

            let hash = header.hash();
            let number = header.number();
            let parent_hash = header.parent_hash();
            if SyncStorage::is_block_hash_confirmed(hash, false) {
                if let Ok(ref mut downloaded_blocks) = SyncStorage::get_downloaded_blocks().lock() {
                    if number == 1 || number == SyncStorage::get_starting_block_number() {
                    } else if let Some(parent_bw) = downloaded_blocks.get_mut(&(number - 1)) {
                        if parent_bw
                            .block_hashes
                            .iter()
                            .filter(|h| *h == parent_hash)
                            .next()
                            .is_none()
                        {
                            continue;
                        }
                    } else {
                        debug!(target: "sync", "number {}, starting_block_number: {}", number, SyncStorage::get_starting_block_number());
                        continue;
                    }

                    if let Some(bw_old) = downloaded_blocks.get_mut(&number) {
                        if &bw_old.parent_hash == parent_hash {
                            let mut index = 0;
                            for h in bw_old.block_hashes.iter() {
                                if h == &hash {
                                    debug!(target: "sync", "Already imported block header #{}-{}", number, hash);
                                    continue;
                                }
                                index += 1;
                            }

                            if index == bw_old.block_hashes.len() {
                                bw_old.block_hashes.extend(vec![hash]);

                                count += 1;
                                local_status.total_difficulty =
                                    local_status.total_difficulty + header.difficulty().clone();
                                local_status.synced_block_number = number;
                                local_status.synced_block_hash = hash;
                                debug!(target: "sync", "Block header #{} - {:?} imported(side chain against {:?}).", number, hash, bw_old.block_hashes);
                            }
                        }
                        continue;
                    }

                    let bw = BlockWrapper {
                        block_number: number,
                        parent_hash: header.parent_hash().clone(),
                        block_hashes: vec![hash],
                        block_headers: None,
                    };

                    downloaded_blocks.insert(number, bw);

                    count += 1;
                    if number > 0 {
                        hw.node_hash = node_hash;
                        hw.hashes.push(hash);
                        hw.headers.push(header.clone());
                    }
                    local_status.total_difficulty =
                        local_status.total_difficulty + header.difficulty().clone();
                    local_status.synced_block_number = number;
                    local_status.synced_block_hash = hash;

                    debug!(target: "sync", "Block header #{} - {:?} imported", number, hash);
                }
            } else {
                warn!(target: "sync", "Not confirmed Block header #{} - {:?}.", number, hash);
            }
        }

        if count > 0 {
            SyncStorage::set_local_status(local_status);
            // if hw.hashes.len() > 0 {
            //     BlockBodiesHandler::send_blocks_bodies_req(node_hash, hw);
            // }
        }
    }
}
