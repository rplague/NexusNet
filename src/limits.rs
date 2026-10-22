// NexusNet - OAHD 计划的核心网络层
//
// Copyright (C) 2026 OAHD
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Shared DHT record limits.
//!
//! libp2p-kad defaults are far smaller than the documents NexusNet stores:
//! the wire packet limit is 16 KiB and the in-memory store accepts values
//! smaller than 64 KiB. Auth and service-type documents can be larger, so both
//! limits are raised here from a single source of truth.

/// Maximum size of a single DHT record value.
pub const DHT_MAX_RECORD_BYTES: usize = 1 << 20;

/// Maximum size of one Kademlia wire packet.
///
/// A record value plus protobuf framing and the closer-peer list must fit.
pub const KAD_MAX_PACKET_BYTES: usize = DHT_MAX_RECORD_BYTES + 64 * 1024;
