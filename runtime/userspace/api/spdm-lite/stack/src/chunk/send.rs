// Licensed under the Apache-2.0 license

//! CHUNK_SEND large-request reassembly.

use mcu_spdm_lite_codec::{
    CapabilitiesBody, ChunkSendAckBody, ChunkSendReqBody, ReqRespCode, SpdmMsgHdrPdu, SpdmVersion,
    WireWriter, CHUNK_ACK_ATTR_EARLY_ERROR, CHUNK_ATTR_LAST_CHUNK,
};
use mcu_spdm_lite_traits::{PalBytes, SpdmPal, SpdmPalIo, SpdmPalIoTransport, SpdmVdmBackend};
use zerocopy::{little_endian::U16, FromBytes};

use crate::build::alloc_padded;
use crate::error::{
    SpdmError, SpdmResult, SPDM_INVALID_REQUEST, SPDM_UNEXPECTED_REQUEST, SPDM_UNSUPPORTED_REQUEST,
    SPDM_VERSION_MISMATCH,
};
use crate::stack::{ConnectionState, Phase};
use crate::vendor_defined;

struct ChunkInfo {
    handle: u8,
    chunk_seq_num: u16,
    complete: bool,
}

pub(crate) async fn handle_chunk_send<'a, Pal, Vdm>(
    state: &mut ConnectionState<Pal::State>,
    pal: &'a Pal,
    io: &<Pal as SpdmPalIoTransport>::Io<'_>,
    vdm_backend: &Vdm,
) -> SpdmResult<PalBytes<'a, Pal>>
where
    Pal: SpdmPal,
    Vdm: SpdmVdmBackend,
{
    let result = process_chunk_send(state, pal, io);
    match result {
        Ok(info) if info.complete => {
            let rsp = build_completed_chunk_send_ack(state, pal, io, info, vdm_backend).await;
            // Reassembly consumed: release the pinned buffer (free + zero).
            pal.large_end();
            rsp
        }
        Ok(info) => build_chunk_send_ack(
            pal,
            io,
            state.version,
            false,
            info.handle,
            info.chunk_seq_num,
            &[],
        ),
        Err(ChunkProcessError::Spdm(e)) => Err(e),
        Err(ChunkProcessError::Early {
            handle,
            chunk_seq_num,
        }) => {
            let mut error = [0u8; 4];
            encode_error_pdu(state.version, SPDM_INVALID_REQUEST, &mut error);
            state.chunk.reset();
            // Release any pinned reassembly buffer reserved before the error.
            pal.large_end();
            build_chunk_send_ack(pal, io, state.version, true, handle, chunk_seq_num, &error)
        }
    }
}

fn build_chunk_send_ack<'a, Pal: SpdmPal>(
    pal: &'a Pal,
    io: &<Pal as SpdmPalIoTransport>::Io<'_>,
    version: SpdmVersion,
    early_error: bool,
    handle: u8,
    chunk_seq_num: u16,
    response_to_large_request: &[u8],
) -> SpdmResult<PalBytes<'a, Pal>> {
    let head = pal.header_size();
    let raw_len =
        head + SpdmMsgHdrPdu::SIZE + ChunkSendAckBody::SIZE + response_to_large_request.len();
    let mut rsp = alloc_padded(pal, io, raw_len)?;
    let mut w = WireWriter::new(&mut rsp[head..]);
    w.write(&SpdmMsgHdrPdu::new(version, ReqRespCode::CHUNK_SEND_ACK))?;
    w.write(&ChunkSendAckBody {
        chunk_receiver_attr: if early_error {
            CHUNK_ACK_ATTR_EARLY_ERROR
        } else {
            0
        },
        handle,
        chunk_seq_num: U16::new(chunk_seq_num),
    })?;
    w.write_bytes(response_to_large_request)?;
    Ok(rsp)
}

/// Maximum bytes we are willing to carry as `ResponseToLargeRequest` inside a
/// CHUNK_SEND_ACK. Large requests primarily cover commands such as debug unlock
/// token programming, whose response is only a small VDM completion. Keeping
/// this bounded avoids reserving another transport-sized scratch buffer while
/// the reassembled request is also live.
const LARGE_REQUEST_RESPONSE_BUF_SIZE: usize = 512;

async fn build_completed_chunk_send_ack<'a, Pal, Vdm>(
    state: &mut ConnectionState<Pal::State>,
    pal: &'a Pal,
    io: &<Pal as SpdmPalIoTransport>::Io<'_>,
    info: ChunkInfo,
    vdm_backend: &Vdm,
) -> SpdmResult<PalBytes<'a, Pal>>
where
    Pal: SpdmPal,
    Vdm: SpdmVdmBackend,
{
    let mut response_to_large_request = [0u8; LARGE_REQUEST_RESPONSE_BUF_SIZE];
    let response_len = match build_response_to_large_request(
        state,
        pal,
        io,
        vdm_backend,
        &mut response_to_large_request,
    )
    .await
    {
        Ok(len) => len,
        Err(err) => {
            let mut error = [0u8; 4];
            encode_error_pdu(state.version, err, &mut error);
            response_to_large_request[..error.len()].copy_from_slice(&error);
            error.len()
        }
    };
    state.chunk.reset();
    build_chunk_send_ack(
        pal,
        io,
        state.version,
        false,
        info.handle,
        info.chunk_seq_num,
        &response_to_large_request[..response_len],
    )
}

enum ChunkProcessError {
    Spdm(SpdmError),
    Early { handle: u8, chunk_seq_num: u16 },
}

fn process_chunk_send<Pal: SpdmPal>(
    state: &mut ConnectionState<Pal::State>,
    pal: &Pal,
    io: &impl SpdmPalIo,
) -> Result<ChunkInfo, ChunkProcessError> {
    if state.large_response.in_progress()
        || (state.phase as u8) < (Phase::AfterCapabilities as u8)
        || !state.chunking_enabled()
    {
        return Err(ChunkProcessError::Spdm(SPDM_UNEXPECTED_REQUEST));
    }

    let req = io.request();
    if req.len() > pal.mtu() {
        return Err(ChunkProcessError::Spdm(SPDM_INVALID_REQUEST));
    }

    let (hdr, body) = SpdmMsgHdrPdu::ref_from_prefix(req)
        .map_err(|_| ChunkProcessError::Spdm(SPDM_INVALID_REQUEST))?;
    if hdr.version != state.version.to_u8() {
        return Err(ChunkProcessError::Spdm(SPDM_VERSION_MISMATCH));
    }

    let (chunk_req, rest) = ChunkSendReqBody::ref_from_prefix(body)
        .map_err(|_| ChunkProcessError::Spdm(SPDM_INVALID_REQUEST))?;
    let handle = chunk_req.handle;
    let chunk_seq_num = chunk_req.chunk_seq_num.get();
    let chunk_size = chunk_req.chunk_size.get() as usize;
    let last_chunk = (chunk_req.chunk_sender_attr & CHUNK_ATTR_LAST_CHUNK) != 0;
    if chunk_req.reserved.get() != 0 || (chunk_req.chunk_sender_attr & !CHUNK_ATTR_LAST_CHUNK) != 0
    {
        return Err(ChunkProcessError::Early {
            handle,
            chunk_seq_num,
        });
    }

    if !state.chunk.in_use {
        process_first_chunk(
            state,
            pal,
            handle,
            chunk_seq_num,
            chunk_size,
            last_chunk,
            rest,
        )?;
    } else {
        process_next_chunk(
            state,
            pal,
            handle,
            chunk_seq_num,
            chunk_size,
            last_chunk,
            rest,
        )?;
    }

    Ok(ChunkInfo {
        handle,
        chunk_seq_num,
        complete: state.chunk.in_use && state.chunk.bytes_received == state.chunk.large_msg_size,
    })
}

fn process_first_chunk<Pal: SpdmPal>(
    state: &mut ConnectionState<Pal::State>,
    pal: &Pal,
    handle: u8,
    chunk_seq_num: u16,
    chunk_size: usize,
    last_chunk: bool,
    rest: &[u8],
) -> Result<(), ChunkProcessError> {
    let Some(size_bytes) = rest.get(..4) else {
        return Err(ChunkProcessError::Early {
            handle,
            chunk_seq_num,
        });
    };
    let mut large_msg_size = [0u8; 4];
    large_msg_size.copy_from_slice(size_bytes);
    let large_msg_size = u32::from_le_bytes(large_msg_size) as usize;
    let chunk_data = &rest[4..];
    let Some(chunk) = chunk_data.get(..chunk_size) else {
        return Err(ChunkProcessError::Early {
            handle,
            chunk_seq_num,
        });
    };
    if chunk_data.len() != chunk_size {
        return Err(ChunkProcessError::Early {
            handle,
            chunk_seq_num,
        });
    };
    let min_chunk_size = CapabilitiesBody::MIN_DATA_TRANSFER_SIZE as usize
        - SpdmMsgHdrPdu::SIZE
        - ChunkSendReqBody::SIZE
        - 4;

    let invalid = chunk_seq_num != 0
        || last_chunk
        || chunk_size < min_chunk_size
        || chunk_size >= large_msg_size
        || large_msg_size <= CapabilitiesBody::MIN_DATA_TRANSFER_SIZE as usize
        || large_msg_size > pal.large_capacity();
    if invalid {
        return Err(ChunkProcessError::Early {
            handle,
            chunk_seq_num,
        });
    }
    // Reserve the pinned reassembly buffer, then store the first chunk into it.
    if pal.large_begin(large_msg_size).is_err() || pal.large_write(0, chunk).is_err() {
        return Err(ChunkProcessError::Early {
            handle,
            chunk_seq_num,
        });
    }

    state.chunk = super::ChunkState {
        in_use: true,
        handle,
        seq_num: 0,
        bytes_received: chunk_size as u32,
        large_msg_size: large_msg_size as u32,
    };
    Ok(())
}

fn process_next_chunk<Pal: SpdmPal>(
    state: &mut ConnectionState<Pal::State>,
    pal: &Pal,
    handle: u8,
    chunk_seq_num: u16,
    chunk_size: usize,
    last_chunk: bool,
    rest: &[u8],
) -> Result<(), ChunkProcessError> {
    let bytes_received = state.chunk.bytes_received as usize;
    let large_msg_size = state.chunk.large_msg_size as usize;
    let end = bytes_received.saturating_add(chunk_size);
    let Some(chunk) = rest.get(..chunk_size) else {
        return Err(ChunkProcessError::Early {
            handle,
            chunk_seq_num,
        });
    };
    if rest.len() != chunk_size {
        return Err(ChunkProcessError::Early {
            handle,
            chunk_seq_num,
        });
    };
    let min_chunk_size = CapabilitiesBody::MIN_DATA_TRANSFER_SIZE as usize
        - SpdmMsgHdrPdu::SIZE
        - ChunkSendReqBody::SIZE;
    let invalid = chunk_seq_num == 0
        || state.chunk.handle != handle
        || state.chunk.seq_num.wrapping_add(1) != chunk_seq_num
        || end > large_msg_size
        || (last_chunk && end != large_msg_size)
        || (!last_chunk && (end >= large_msg_size || chunk_size < min_chunk_size));
    if invalid || pal.large_write(bytes_received, chunk).is_err() {
        return Err(ChunkProcessError::Early {
            handle,
            chunk_seq_num,
        });
    }

    state.chunk.seq_num = chunk_seq_num;
    state.chunk.bytes_received = end as u32;
    Ok(())
}

async fn build_response_to_large_request<Pal, Vdm>(
    state: &ConnectionState<Pal::State>,
    pal: &Pal,
    io: &<Pal as SpdmPalIoTransport>::Io<'_>,
    vdm_backend: &Vdm,
    out: &mut [u8],
) -> SpdmResult<usize>
where
    Pal: SpdmPal,
    Vdm: SpdmVdmBackend,
{
    let large_req_len = state.chunk.large_msg_size as usize;
    if large_req_len < SpdmMsgHdrPdu::SIZE {
        return Err(SPDM_INVALID_REQUEST);
    }
    let mut large_req = pal.alloc_bytes(io, large_req_len)?;
    pal.large_read(0, &mut large_req)
        .map_err(|_| SPDM_INVALID_REQUEST)?;
    let (hdr, _) = SpdmMsgHdrPdu::ref_from_prefix(&large_req).map_err(|_| SPDM_INVALID_REQUEST)?;
    if hdr.version != state.version.to_u8()
        || hdr.code == ReqRespCode::CHUNK_SEND
        || hdr.code == ReqRespCode::CHUNK_GET
    {
        return Err(SPDM_INVALID_REQUEST);
    }
    match hdr.code {
        ReqRespCode::VENDOR_DEFINED_REQUEST => {
            vendor_defined::handle_large_vendor_defined_request(
                vdm_backend,
                state,
                pal,
                io,
                &large_req,
                false,
                out,
            )
            .await
        }
        _ => Err(SPDM_UNSUPPORTED_REQUEST),
    }
}

fn encode_error_pdu(version: SpdmVersion, err: SpdmError, out: &mut [u8; 4]) {
    out[0] = version.to_u8();
    out[1] = ReqRespCode::ERROR.0;
    out[2] = err.spec_byte();
    out[3] = err.error_data();
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::error::SPDM_UNSPECIFIED;
    use crate::stack::{ConnectionState, Phase};
    use core::cell::RefCell;
    use core::marker::PhantomData;
    use core::ops::{Deref, DerefMut};
    use futures::executor::block_on;
    use mcu_spdm_lite_codec::{
        CapFlags, HashAlgos, MeasHashAlgos, SpdmVersion, StandardsBodyId,
        CHUNK_ACK_ATTR_EARLY_ERROR,
    };
    use mcu_spdm_lite_traits::{
        McuResult, MeasurementInfo, SpdmPal, SpdmPalAlloc, SpdmPalAsymAlgo, SpdmPalCertStore,
        SpdmPalHash, SpdmPalHashAlgo, SpdmPalIo, SpdmPalIoKind, SpdmPalIoTransport,
        SpdmPalMeasurements, SpdmPalSessionCrypto, SpdmVdmBackend, VdmRegistry, VdmResponse,
        VdmResponseBuffer, SPDM_NONCE_LEN,
    };
    use std::boxed::Box;
    use std::vec;
    use std::vec::Vec;

    const TEST_MTU: usize = 64;
    const LARGE_CAPACITY: usize = 16 * 1024;
    const HANDLE: u8 = 0x5a;
    const OCP_CALIPTRA_VENDOR_ID: u32 = 42_623;
    const CALIPTRA_VDM_COMMAND_VERSION: u8 = 0x01;
    const AUTHORIZE_DEBUG_UNLOCK_TOKEN: u8 = 0x0b;

    #[derive(Clone)]
    struct TestHashState([u8; 48]);

    impl Default for TestHashState {
        fn default() -> Self {
            Self([0; 48])
        }
    }

    struct TestIo {
        request: Vec<u8>,
    }

    impl SpdmPalIo for TestIo {
        fn kind(&self) -> SpdmPalIoKind {
            SpdmPalIoKind::Message
        }

        fn request(&self) -> &[u8] {
            &self.request
        }
    }

    struct TestBox<'a, T: 'a> {
        value: Box<T>,
        _lifetime: PhantomData<&'a ()>,
    }

    impl<T> Deref for TestBox<'_, T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            &self.value
        }
    }

    impl<T> DerefMut for TestBox<'_, T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.value
        }
    }

    struct TestPal {
        mtu: usize,
        large_capacity: usize,
        large: RefCell<Option<Vec<u8>>>,
    }

    impl Default for TestPal {
        fn default() -> Self {
            Self {
                mtu: TEST_MTU,
                large_capacity: LARGE_CAPACITY,
                large: RefCell::new(None),
            }
        }
    }

    impl SpdmPalAlloc for TestPal {
        type Box<'a, T>
            = TestBox<'a, T>
        where
            Self: 'a,
            T: 'a;
        type Bytes<'a>
            = Vec<u8>
        where
            Self: 'a;

        fn alloc<T: Sized>(&self, _io: &impl SpdmPalIo, value: T) -> McuResult<Self::Box<'_, T>> {
            Ok(TestBox {
                value: Box::new(value),
                _lifetime: PhantomData,
            })
        }

        fn alloc_bytes(&self, _io: &impl SpdmPalIo, len: usize) -> McuResult<Self::Bytes<'_>> {
            Ok(vec![0; len])
        }

        fn large_capacity(&self) -> usize {
            self.large_capacity
        }

        fn large_begin(&self, len: usize) -> McuResult<()> {
            if len > self.large_capacity {
                return Err(mcu_error::codes::OUT_OF_MEMORY);
            }
            self.large.replace(Some(vec![0; len]));
            Ok(())
        }

        fn large_write(&self, offset: usize, data: &[u8]) -> McuResult<()> {
            let mut large = self.large.borrow_mut();
            let Some(buf) = large.as_mut() else {
                return Err(mcu_error::codes::INVARIANT);
            };
            let end = offset
                .checked_add(data.len())
                .ok_or(mcu_error::codes::INVARIANT)?;
            if end > buf.len() {
                return Err(mcu_error::codes::INVARIANT);
            }
            buf[offset..end].copy_from_slice(data);
            Ok(())
        }

        fn large_read(&self, offset: usize, out: &mut [u8]) -> McuResult<()> {
            let large = self.large.borrow();
            let Some(buf) = large.as_ref() else {
                return Err(mcu_error::codes::INVARIANT);
            };
            let end = offset
                .checked_add(out.len())
                .ok_or(mcu_error::codes::INVARIANT)?;
            if end > buf.len() {
                return Err(mcu_error::codes::INVARIANT);
            }
            out.copy_from_slice(&buf[offset..end]);
            Ok(())
        }

        fn large_end(&self) {
            self.large.replace(None);
        }
    }

    impl SpdmPalIoTransport for TestPal {
        type Io<'a>
            = TestIo
        where
            Self: 'a;

        fn secure_message_supported(&self) -> bool {
            false
        }

        fn header_size(&self) -> usize {
            0
        }

        fn mtu(&self) -> usize {
            self.mtu
        }

        async fn recv_request(&self) -> McuResult<Self::Io<'_>> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn send_response(
            &self,
            _io: &Self::Io<'_>,
            _kind: SpdmPalIoKind,
            _msg: &mut [u8],
        ) -> McuResult<()> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }
    }

    impl SpdmPalHash for TestPal {
        type State = TestHashState;

        async fn hash_init(
            &self,
            _io: &impl SpdmPalIo,
            _algo: SpdmPalHashAlgo,
            seed: &[u8],
        ) -> McuResult<Self::State> {
            let mut state = TestHashState::default();
            state.0[0] = seed.len() as u8;
            Ok(state)
        }

        async fn hash_update(
            &self,
            _io: &impl SpdmPalIo,
            state: &mut Self::State,
            data: &[u8],
        ) -> McuResult<()> {
            state.0[0] = state.0[0].wrapping_add(data.len() as u8);
            Ok(())
        }

        fn hash_clone(&self, _io: &impl SpdmPalIo, state: &Self::State) -> McuResult<Self::State> {
            Ok(state.clone())
        }

        async fn hash_finish(
            &self,
            _io: &impl SpdmPalIo,
            state: &mut Self::State,
            out: &mut [u8],
        ) -> McuResult<()> {
            out[..48].copy_from_slice(&state.0);
            Ok(())
        }
    }

    impl SpdmPalCertStore for TestPal {
        fn supported_slots(&self) -> u8 {
            0
        }

        fn provisioned_slots(&self) -> u8 {
            0
        }

        async fn cert_chain_len(
            &self,
            _io: &Self::Io<'_>,
            _slot: u8,
            _algo: SpdmPalAsymAlgo,
        ) -> McuResult<usize> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn root_cert_hash(
            &self,
            _io: &Self::Io<'_>,
            _slot: u8,
            _algo: SpdmPalAsymAlgo,
            _hash_algo: SpdmPalHashAlgo,
            _out: &mut [u8],
        ) -> McuResult<()> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn read_cert_chain(
            &self,
            _io: &Self::Io<'_>,
            _slot: u8,
            _algo: SpdmPalAsymAlgo,
            _offset: usize,
            _dst: &mut [u8],
        ) -> McuResult<usize> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn sign_hash(
            &self,
            _io: &Self::Io<'_>,
            _slot: u8,
            _algo: SpdmPalAsymAlgo,
            _digest: &[u8],
            _signature: &mut [u8],
        ) -> McuResult<usize> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn write_cert_chain(
            &self,
            _io: &Self::Io<'_>,
            _slot: u8,
            _algo: SpdmPalAsymAlgo,
            _key_pair_id: u8,
            _cert_info: u8,
            _root_hash: &[u8; 48],
            _data: &[u8],
        ) -> McuResult<()> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn erase_cert_chain(
            &self,
            _io: &Self::Io<'_>,
            _slot: u8,
            _algo: SpdmPalAsymAlgo,
        ) -> McuResult<()> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        fn key_pair_id(&self, _slot: u8) -> Option<u8> {
            None
        }

        fn cert_info(&self, _slot: u8) -> Option<u8> {
            None
        }

        fn key_usage_mask(&self, _slot: u8) -> Option<u16> {
            None
        }

        async fn generate_nonce(&self, _io: &Self::Io<'_>, out: &mut [u8]) -> McuResult<()> {
            out.fill(0xa5);
            Ok(())
        }
    }

    impl SpdmPalMeasurements for TestPal {
        fn measurement_info(&self) -> &[MeasurementInfo] {
            &[]
        }

        async fn get_measurement_value(
            &self,
            _io: &Self::Io<'_>,
            _index: u8,
            _nonce: Option<&[u8; SPDM_NONCE_LEN]>,
            _out: &mut [u8],
        ) -> McuResult<usize> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }
    }

    impl SpdmPalSessionCrypto for TestPal {
        type Key = [u8; 48];

        async fn ecdh_generate(
            &self,
            _io: &impl SpdmPalIo,
            _context: &mut [u8],
            _exchange_data: &mut [u8],
        ) -> McuResult<()> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn ecdh_finish(
            &self,
            _io: &impl SpdmPalIo,
            _context: &[u8],
            _peer_exchange_data: &[u8],
        ) -> McuResult<Self::Key> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn hkdf_extract_bytes(
            &self,
            _io: &impl SpdmPalIo,
            _salt: &[u8],
            _ikm: &Self::Key,
        ) -> McuResult<Self::Key> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn hkdf_extract_key(
            &self,
            _io: &impl SpdmPalIo,
            _salt: &Self::Key,
            _ikm: &Self::Key,
        ) -> McuResult<Self::Key> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn hkdf_expand(
            &self,
            _io: &impl SpdmPalIo,
            _prk: &Self::Key,
            _key_size: u32,
            _info: &[u8],
        ) -> McuResult<Self::Key> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn hmac(
            &self,
            _io: &impl SpdmPalIo,
            _key: &Self::Key,
            _data: &[u8],
            _out: &mut [u8],
        ) -> McuResult<usize> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn import_key(&self, _io: &impl SpdmPalIo, _data: &[u8]) -> McuResult<Self::Key> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn aead_encrypt(
            &self,
            _io: &impl SpdmPalIo,
            _key: &Self::Key,
            _spdm_version: u8,
            _seq: u64,
            _aad: &[u8],
            _plaintext: &[u8],
            _ciphertext: &mut [u8],
        ) -> McuResult<(usize, [u8; 16])> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }

        async fn aead_decrypt(
            &self,
            _io: &impl SpdmPalIo,
            _key: &Self::Key,
            _spdm_version: u8,
            _seq: u64,
            _aad: &[u8],
            _ciphertext: &[u8],
            _tag: &[u8; 16],
            _plaintext: &mut [u8],
        ) -> McuResult<usize> {
            Err(mcu_error::codes::NOT_IMPLEMENTED)
        }
    }

    impl SpdmPal for TestPal {}

    enum VdmBehavior {
        Success,
        LargeResponse,
    }

    struct DebugUnlockVdm {
        behavior: VdmBehavior,
        matched: bool,
        seen_token: RefCell<Option<Vec<u8>>>,
    }

    impl DebugUnlockVdm {
        fn success() -> Self {
            Self {
                behavior: VdmBehavior::Success,
                matched: true,
                seen_token: RefCell::new(None),
            }
        }

        fn large_response() -> Self {
            Self {
                behavior: VdmBehavior::LargeResponse,
                matched: true,
                seen_token: RefCell::new(None),
            }
        }

        fn no_match() -> Self {
            Self {
                behavior: VdmBehavior::Success,
                matched: false,
                seen_token: RefCell::new(None),
            }
        }
    }

    impl SpdmVdmBackend for DebugUnlockVdm {
        const USES_LARGE_RESPONSE: bool = true;
        const LARGE_RESPONSE_CAPACITY: usize = 1024;

        fn match_id(&self, registry: &VdmRegistry<'_>) -> bool {
            self.matched
                && registry.standard_id == StandardsBodyId::Iana.as_u16()
                && registry.vendor_id == OCP_CALIPTRA_VENDOR_ID.to_le_bytes()
        }

        async fn handle_request<Alloc, Io>(
            &self,
            req: &[u8],
            rsp: VdmResponseBuffer<'_, Alloc, Io>,
        ) -> McuResult<VdmResponse>
        where
            Alloc: SpdmPalAlloc,
            Io: SpdmPalIo,
        {
            assert!(req.len() >= 2);
            assert_eq!(req[0], CALIPTRA_VDM_COMMAND_VERSION);
            assert_eq!(req[1], AUTHORIZE_DEBUG_UNLOCK_TOKEN);
            self.seen_token.replace(Some(req[2..].to_vec()));

            match self.behavior {
                VdmBehavior::Success => {
                    rsp.inline[..3].copy_from_slice(&[
                        CALIPTRA_VDM_COMMAND_VERSION,
                        AUTHORIZE_DEBUG_UNLOCK_TOKEN,
                        0,
                    ]);
                    Ok(VdmResponse::Inline(3))
                }
                VdmBehavior::LargeResponse => Ok(VdmResponse::Large(3)),
            }
        }
    }

    fn state() -> ConnectionState<TestHashState> {
        let mut state = ConnectionState::caliptra();
        state.phase = Phase::AfterCapabilities;
        state.version = SpdmVersion::V12;
        state.peer_data_transfer_size = TEST_MTU as u32;
        state.peer_max_spdm_msg_size = LARGE_CAPACITY as u32;
        state.peer_cap_flags = CapFlags::CHUNK;
        state.advertised_cap_flags = state.cap_flags;
        state.negotiated_base_hash_sel = HashAlgos::SHA_384;
        state.meas_hash_algo = MeasHashAlgos::SHA_384;
        state
    }

    fn vendor_defined_debug_unlock_request(token: &[u8]) -> Vec<u8> {
        let vendor_id = OCP_CALIPTRA_VENDOR_ID.to_le_bytes();
        let req_len = 2 + token.len();
        let mut msg = Vec::new();
        msg.extend_from_slice(&[
            SpdmVersion::V12.to_u8(),
            ReqRespCode::VENDOR_DEFINED_REQUEST.0,
            0,
            0,
        ]);
        msg.extend_from_slice(&StandardsBodyId::Iana.as_u16().to_le_bytes());
        msg.push(vendor_id.len() as u8);
        msg.extend_from_slice(&vendor_id);
        msg.extend_from_slice(&(req_len as u16).to_le_bytes());
        msg.extend_from_slice(&[CALIPTRA_VDM_COMMAND_VERSION, AUTHORIZE_DEBUG_UNLOCK_TOKEN]);
        msg.extend_from_slice(token);
        msg
    }

    fn chunk_frame(
        full_msg: &[u8],
        offset: usize,
        chunk_size: usize,
        seq: u16,
        handle: u8,
        last: bool,
    ) -> TestIo {
        let mut req = Vec::new();
        req.extend_from_slice(&[
            SpdmVersion::V12.to_u8(),
            ReqRespCode::CHUNK_SEND.0,
            if last { CHUNK_ATTR_LAST_CHUNK } else { 0 },
            handle,
        ]);
        req.extend_from_slice(&seq.to_le_bytes());
        req.extend_from_slice(&0u16.to_le_bytes());
        req.extend_from_slice(&(chunk_size as u32).to_le_bytes());
        if seq == 0 {
            req.extend_from_slice(&(full_msg.len() as u32).to_le_bytes());
        }
        req.extend_from_slice(&full_msg[offset..offset + chunk_size]);
        TestIo { request: req }
    }

    fn chunk_frames(full_msg: &[u8], handle: u8) -> Vec<TestIo> {
        let first_max = TEST_MTU - SpdmMsgHdrPdu::SIZE - ChunkSendReqBody::SIZE - 4;
        let next_max = TEST_MTU - SpdmMsgHdrPdu::SIZE - ChunkSendReqBody::SIZE;
        let mut frames = Vec::new();
        let mut offset = 0usize;
        let mut seq = 0u16;

        let first = first_max.min(full_msg.len() - 1);
        frames.push(chunk_frame(full_msg, offset, first, seq, handle, false));
        offset += first;
        seq += 1;

        while offset < full_msg.len() {
            let remaining = full_msg.len() - offset;
            let n = remaining.min(next_max);
            frames.push(chunk_frame(
                full_msg,
                offset,
                n,
                seq,
                handle,
                n == remaining,
            ));
            offset += n;
            seq += 1;
        }

        frames
    }

    fn run_frame(
        state: &mut ConnectionState<TestHashState>,
        pal: &TestPal,
        io: &TestIo,
        vdm: &DebugUnlockVdm,
    ) -> SpdmResult<Vec<u8>> {
        block_on(handle_chunk_send(state, pal, io, vdm)).map(|rsp| rsp.to_vec())
    }

    fn ack_parts(rsp: &[u8]) -> (u8, u8, u16, &[u8]) {
        assert!(rsp.len() >= SpdmMsgHdrPdu::SIZE + ChunkSendAckBody::SIZE);
        assert_eq!(rsp[0], SpdmVersion::V12.to_u8());
        assert_eq!(rsp[1], ReqRespCode::CHUNK_SEND_ACK.0);
        let attrs = rsp[2];
        let handle = rsp[3];
        let seq = u16::from_le_bytes([rsp[4], rsp[5]]);
        (attrs, handle, seq, &rsp[6..])
    }

    fn assert_empty_ack(rsp: &[u8], seq: u16) {
        let (attrs, handle, got_seq, rest) = ack_parts(rsp);
        assert_eq!(attrs, 0);
        assert_eq!(handle, HANDLE);
        assert_eq!(got_seq, seq);
        assert!(rest.is_empty());
    }

    fn assert_early_invalid_request(rsp: &[u8], handle: u8, seq: u16) {
        let (attrs, got_handle, got_seq, rest) = ack_parts(rsp);
        assert_eq!(attrs, CHUNK_ACK_ATTR_EARLY_ERROR);
        assert_eq!(got_handle, handle);
        assert_eq!(got_seq, seq);
        assert_eq!(
            rest,
            &[
                SpdmVersion::V12.to_u8(),
                ReqRespCode::ERROR.0,
                SPDM_INVALID_REQUEST.spec_byte(),
                SPDM_INVALID_REQUEST.error_data(),
            ]
        );
    }

    fn assert_reassembly_aborted(state: &ConnectionState<TestHashState>, pal: &TestPal) {
        assert!(!state.chunk.in_progress());
        assert!(pal.large.borrow().is_none());
    }

    fn assert_response_to_large_request_success(rsp: &[u8]) {
        assert_eq!(rsp[0], SpdmVersion::V12.to_u8());
        assert_eq!(rsp[1], ReqRespCode::VENDOR_DEFINED_RESPONSE.0);
        assert_eq!(&rsp[2..4], &[0, 0]);
        assert_eq!(
            u16::from_le_bytes([rsp[4], rsp[5]]),
            StandardsBodyId::Iana.as_u16()
        );
        assert_eq!(rsp[6], 4);
        assert_eq!(&rsp[7..11], &OCP_CALIPTRA_VENDOR_ID.to_le_bytes());
        assert_eq!(u16::from_le_bytes([rsp[11], rsp[12]]), 3);
        assert_eq!(
            &rsp[13..16],
            &[
                CALIPTRA_VDM_COMMAND_VERSION,
                AUTHORIZE_DEBUG_UNLOCK_TOKEN,
                0
            ]
        );
        assert_eq!(rsp.len(), 16);
    }

    fn assert_response_to_large_request_error(rsp: &[u8], err: SpdmError) {
        assert_eq!(
            rsp,
            &[
                SpdmVersion::V12.to_u8(),
                ReqRespCode::ERROR.0,
                err.spec_byte(),
                err.error_data(),
            ]
        );
    }

    #[test]
    fn chunk_send_reassembles_caliptra_authorize_debug_unlock_vdm_request() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let token: Vec<u8> = (0..7_500).map(|i| (i & 0xff) as u8).collect();
        let large_req = vendor_defined_debug_unlock_request(&token);
        let frames = chunk_frames(&large_req, HANDLE);

        assert!(frames.len() > 2);
        for (seq, frame) in frames[..frames.len() - 1].iter().enumerate() {
            let rsp = run_frame(&mut state, &pal, frame, &vdm).unwrap();
            assert_empty_ack(&rsp, seq as u16);
        }

        let final_seq = (frames.len() - 1) as u16;
        let rsp = run_frame(&mut state, &pal, frames.last().unwrap(), &vdm).unwrap();
        let (attrs, handle, seq, response_to_large_request) = ack_parts(&rsp);
        assert_eq!(attrs, 0);
        assert_eq!(handle, HANDLE);
        assert_eq!(seq, final_seq);
        assert_response_to_large_request_success(response_to_large_request);
        assert_eq!(vdm.seen_token.take(), Some(token));
        assert!(pal.large.borrow().is_none());
        assert!(!state.chunk.in_progress());
    }

    #[test]
    fn chunk_send_response_to_large_request_rejects_large_vdm_response() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::large_response();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let frames = chunk_frames(&large_req, HANDLE);

        for frame in &frames[..frames.len() - 1] {
            run_frame(&mut state, &pal, frame, &vdm).unwrap();
        }
        let rsp = run_frame(&mut state, &pal, frames.last().unwrap(), &vdm).unwrap();
        let (_, _, _, response_to_large_request) = ack_parts(&rsp);
        assert_response_to_large_request_error(response_to_large_request, SPDM_UNSPECIFIED);
        assert!(pal.large.borrow().is_none());
    }

    #[test]
    fn chunk_send_unsupported_vendor_defined_registry_returns_error_in_response_to_large_request() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::no_match();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let frames = chunk_frames(&large_req, HANDLE);

        for frame in &frames[..frames.len() - 1] {
            run_frame(&mut state, &pal, frame, &vdm).unwrap();
        }
        let rsp = run_frame(&mut state, &pal, frames.last().unwrap(), &vdm).unwrap();
        let (_, _, _, response_to_large_request) = ack_parts(&rsp);
        assert_response_to_large_request_error(
            response_to_large_request,
            SPDM_UNSUPPORTED_REQUEST.with_data(ReqRespCode::VENDOR_DEFINED_REQUEST.0),
        );
    }

    #[test]
    fn chunk_send_without_chunk_cap_returns_unexpected_request_error() {
        let pal = TestPal::default();
        let mut state = state();
        state.peer_cap_flags = CapFlags::EMPTY;
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let frame = chunk_frames(&large_req, HANDLE).remove(0);

        assert_eq!(
            run_frame(&mut state, &pal, &frame, &vdm).unwrap_err(),
            SPDM_UNEXPECTED_REQUEST
        );
    }

    #[test]
    fn chunk_send_before_after_capabilities_returns_unexpected_request_error() {
        let pal = TestPal::default();
        let mut state = state();
        state.phase = Phase::Start;
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let frame = chunk_frames(&large_req, HANDLE).remove(0);

        assert_eq!(
            run_frame(&mut state, &pal, &frame, &vdm).unwrap_err(),
            SPDM_UNEXPECTED_REQUEST
        );
    }

    #[test]
    fn first_chunk_bad_seq_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let mut frame = chunk_frame(&large_req, 0, 48, 0, HANDLE, false);
        frame.request[SpdmMsgHdrPdu::SIZE + 2..SpdmMsgHdrPdu::SIZE + 4]
            .copy_from_slice(&1u16.to_le_bytes());

        let rsp = run_frame(&mut state, &pal, &frame, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 1);
        assert!(pal.large.borrow().is_none());
    }

    #[test]
    fn first_chunk_last_chunk_set_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let frame = chunk_frame(&large_req, 0, 48, 0, HANDLE, true);

        let rsp = run_frame(&mut state, &pal, &frame, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 0);
    }

    #[test]
    fn first_chunk_with_reserved_field_set_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let mut frame = chunk_frame(&large_req, 0, 48, 0, HANDLE, false);
        frame.request[SpdmMsgHdrPdu::SIZE + 4] = 1;

        let rsp = run_frame(&mut state, &pal, &frame, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 0);
    }

    #[test]
    fn first_chunk_with_unsupported_sender_attribute_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let mut frame = chunk_frame(&large_req, 0, 48, 0, HANDLE, false);
        frame.request[SpdmMsgHdrPdu::SIZE] = 0x80;

        let rsp = run_frame(&mut state, &pal, &frame, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 0);
    }

    #[test]
    fn first_chunk_with_trailing_bytes_beyond_chunk_size_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let mut frame = chunk_frame(&large_req, 0, 46, 0, HANDLE, false);
        frame.request.extend_from_slice(&[0xde, 0xad]);

        let rsp = run_frame(&mut state, &pal, &frame, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 0);
        assert_reassembly_aborted(&state, &pal);
    }

    #[test]
    fn first_chunk_size_exceeds_large_capacity_gets_early_error_ack() {
        let pal = TestPal {
            large_capacity: 128,
            ..TestPal::default()
        };
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let frame = chunk_frame(&large_req, 0, 48, 0, HANDLE, false);

        let rsp = run_frame(&mut state, &pal, &frame, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 0);
    }

    #[test]
    fn first_chunk_too_small_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let min_first = mcu_spdm_lite_codec::CapabilitiesBody::MIN_DATA_TRANSFER_SIZE as usize
            - SpdmMsgHdrPdu::SIZE
            - ChunkSendReqBody::SIZE
            - 4;
        let frame = chunk_frame(&large_req, 0, min_first - 1, 0, HANDLE, false);

        let rsp = run_frame(&mut state, &pal, &frame, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 0);
    }

    #[test]
    fn next_chunk_bad_seq_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let first = chunk_frame(&large_req, 0, 48, 0, HANDLE, false);
        run_frame(&mut state, &pal, &first, &vdm).unwrap();

        let second = chunk_frame(&large_req, 48, 48, 2, HANDLE, false);
        let rsp = run_frame(&mut state, &pal, &second, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 2);
        assert_reassembly_aborted(&state, &pal);
    }

    #[test]
    fn next_chunk_bad_handle_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let first = chunk_frame(&large_req, 0, 48, 0, HANDLE, false);
        run_frame(&mut state, &pal, &first, &vdm).unwrap();

        let second = chunk_frame(&large_req, 48, 48, 1, HANDLE.wrapping_add(1), false);
        let rsp = run_frame(&mut state, &pal, &second, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE.wrapping_add(1), 1);
        assert_reassembly_aborted(&state, &pal);
    }

    #[test]
    fn next_chunk_with_trailing_bytes_beyond_chunk_size_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let first = chunk_frame(&large_req, 0, 48, 0, HANDLE, false);
        run_frame(&mut state, &pal, &first, &vdm).unwrap();

        let mut second = chunk_frame(&large_req, 48, 48, 1, HANDLE, false);
        second.request.extend_from_slice(&[0xde, 0xad]);
        let rsp = run_frame(&mut state, &pal, &second, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 1);
        assert_reassembly_aborted(&state, &pal);
    }

    #[test]
    fn next_chunk_overruns_large_message_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 80]);
        let mut first = chunk_frame(&large_req, 0, 48, 0, HANDLE, false);
        let large_size_offset = SpdmMsgHdrPdu::SIZE + ChunkSendReqBody::SIZE;
        let advertised_too_small = 80u32.to_le_bytes();
        first.request[large_size_offset..large_size_offset + 4]
            .copy_from_slice(&advertised_too_small);
        run_frame(&mut state, &pal, &first, &vdm).unwrap();

        let second = chunk_frame(&large_req, 48, large_req.len() - 48, 1, HANDLE, false);
        let rsp = run_frame(&mut state, &pal, &second, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 1);
        assert_reassembly_aborted(&state, &pal);
    }

    #[test]
    fn next_chunk_last_before_complete_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 256]);
        let first = chunk_frame(&large_req, 0, 48, 0, HANDLE, false);
        run_frame(&mut state, &pal, &first, &vdm).unwrap();

        let second = chunk_frame(&large_req, 48, 48, 1, HANDLE, true);
        let rsp = run_frame(&mut state, &pal, &second, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 1);
        assert_reassembly_aborted(&state, &pal);
    }

    #[test]
    fn next_chunk_missing_last_when_complete_gets_early_error_ack() {
        let pal = TestPal::default();
        let mut state = state();
        let vdm = DebugUnlockVdm::success();
        let large_req = vendor_defined_debug_unlock_request(&[0xa5; 80]);
        let first = chunk_frame(&large_req, 0, 48, 0, HANDLE, false);
        run_frame(&mut state, &pal, &first, &vdm).unwrap();

        let second = chunk_frame(&large_req, 48, large_req.len() - 48, 1, HANDLE, false);
        let rsp = run_frame(&mut state, &pal, &second, &vdm).unwrap();
        assert_early_invalid_request(&rsp, HANDLE, 1);
        assert_reassembly_aborted(&state, &pal);
    }
}
