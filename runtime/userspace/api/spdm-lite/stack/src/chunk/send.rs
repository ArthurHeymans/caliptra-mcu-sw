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
    out[3] = 0;
}
