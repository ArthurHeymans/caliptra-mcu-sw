// Licensed under the Apache-2.0 license

//! Platform implementation of the Caliptra VDM device-operations hook.
//!
//! [`CaliptraVdmHook`] is the emulator's [`CaliptraVdmCommands`] backend: it
//! performs the actual device work (Caliptra mailbox calls) for the Caliptra
//! VDM commands. The protocol/dispatch/framing all live in the
//! `mcu-spdm-lite-vdm-handler` lib; this hook only supplies the device ops.

use caliptra_mcu_libapi_caliptra::certificate::CertContext;
use caliptra_mcu_libapi_caliptra::crypto::asym::AsymAlgo;
use caliptra_mcu_libapi_caliptra::error::CaliptraApiError;
use mcu_spdm_lite_traits::{SpdmPalAlloc, SpdmPalIo};
use mcu_spdm_lite_vdm_handler::iana::ocp::caliptra_vdm::{
    CaliptraCompletionCode, CaliptraVdmCommands, CaliptraVdmResult,
};

/// Emulator Caliptra VDM device-operations backend.
pub struct CaliptraVdmHook;

impl CaliptraVdmCommands for CaliptraVdmHook {
    async fn firmware_version<A: SpdmPalAlloc, I: SpdmPalIo>(
        &self,
        _area_index: u32,
        _scratch: &A,
        _io: &I,
        _out: &mut [u8],
    ) -> CaliptraVdmResult<usize> {
        // No firmware-version device source is wired on this platform yet.
        Err(CaliptraCompletionCode::UnsupportedOperation)
    }

    async fn export_attested_csr<A: SpdmPalAlloc, I: SpdmPalIo>(
        &self,
        device_key_id: u32,
        algorithm: u32,
        nonce: &[u8; 32],
        _scratch: &A,
        _io: &I,
        out: &mut [u8],
    ) -> CaliptraVdmResult<usize> {
        let algo =
            AsymAlgo::try_from_u32(algorithm).ok_or(CaliptraCompletionCode::InvalidParameter)?;
        let mut cert_ctx = CertContext::new();
        cert_ctx
            .get_attested_csr(algo, device_key_id, nonce, out)
            .await
            .map_err(|e| match e {
                CaliptraApiError::MailboxBusy => CaliptraCompletionCode::CaliptraMailboxBusy,
                CaliptraApiError::BufferTooSmall => CaliptraCompletionCode::CaliptraBufferTooSmall,
                CaliptraApiError::InvalidResponse
                | CaliptraApiError::Mailbox(_)
                | CaliptraApiError::Syscall(_) => CaliptraCompletionCode::OperationFailed,
                _ => CaliptraCompletionCode::GeneralError,
            })
    }
}
