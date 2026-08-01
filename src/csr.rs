// Moved from swbt-rs unit_010 into the backend during T06f2. The CSR BCCMD
// layout is retained byte-for-byte so explicit local identities remain an
// implementation detail of backend session opening.

use std::fmt;

pub(super) const CSR_VENDOR_OPCODE: u16 = 0xFC00;

const CSR_BCCMD_CHANNEL: u8 = 0xC2;
const CSR_GETREQ: u16 = 0x0000;
const CSR_GETRESP: u16 = 0x0001;
const CSR_SETREQ: u16 = 0x0002;
const CSR_PSKEY_BDADDR: u16 = 0x0001;
const CSR_VARID_PS: u16 = 0x7003;
const CSR_VOLATILE_STORE: u16 = 0x0008;
const BD_ADDR_VALUE_LENGTH: usize = 8;

pub(super) struct CsrVendorCommand {
    op_code: u16,
    parameters: Box<[u8]>,
}

impl CsrVendorCommand {
    pub(super) const fn op_code(&self) -> u16 {
        self.op_code
    }

    pub(super) fn parameters(&self) -> &[u8] {
        &self.parameters
    }
}

pub(super) struct CsrBdAddrRewritePlan {
    write: CsrVendorCommand,
    reset: CsrVendorCommand,
}

impl CsrBdAddrRewritePlan {
    pub(super) const fn write(&self) -> &CsrVendorCommand {
        &self.write
    }

    pub(super) const fn reset(&self) -> &CsrVendorCommand {
        &self.reset
    }
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct CsrBdAddrReadResponse {
    address: [u8; 6],
    status: u16,
}

#[cfg(test)]
impl CsrBdAddrReadResponse {
    pub(super) const fn address(&self) -> [u8; 6] {
        self.address
    }

    pub(super) const fn status(&self) -> u16 {
        self.status
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CsrCodecError {
    MalformedResponse,
    NotCsrBccmd,
    FailedResponse,
}

impl fmt::Display for CsrCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MalformedResponse => "CSR vendor response is malformed",
            Self::NotCsrBccmd => "vendor response is not CSR BCCMD",
            Self::FailedResponse => "CSR vendor response reported failure",
        })
    }
}

impl std::error::Error for CsrCodecError {}

#[cfg(test)]
pub(super) fn build_csr_bd_addr_read_command(sequence_number: u16) -> CsrVendorCommand {
    let value_words = (BD_ADDR_VALUE_LENGTH / 2) as u16;
    let payload_words = value_words + 8;
    let mut payload = [0_u8; 24];
    payload[0..2].copy_from_slice(&CSR_GETREQ.to_le_bytes());
    payload[2..4].copy_from_slice(&payload_words.to_le_bytes());
    payload[4..6].copy_from_slice(&sequence_number.to_le_bytes());
    payload[6..8].copy_from_slice(&CSR_VARID_PS.to_le_bytes());
    payload[10..12].copy_from_slice(&CSR_PSKEY_BDADDR.to_le_bytes());
    payload[12..14].copy_from_slice(&value_words.to_le_bytes());
    vendor_command(&payload)
}

pub(super) fn build_csr_bd_addr_volatile_rewrite_plan(
    address: [u8; 6],
    sequence_number: u16,
) -> CsrBdAddrRewritePlan {
    let value_words = (BD_ADDR_VALUE_LENGTH / 2) as u16;
    let payload_words = value_words + 8;
    let mut write = [0_u8; 24];
    write[0..2].copy_from_slice(&CSR_SETREQ.to_le_bytes());
    write[2..4].copy_from_slice(&payload_words.to_le_bytes());
    write[4..6].copy_from_slice(&sequence_number.to_le_bytes());
    write[6..8].copy_from_slice(&CSR_VARID_PS.to_le_bytes());
    write[10..12].copy_from_slice(&CSR_PSKEY_BDADDR.to_le_bytes());
    write[12..14].copy_from_slice(&value_words.to_le_bytes());
    write[14..16].copy_from_slice(&CSR_VOLATILE_STORE.to_le_bytes());

    let reversed = [
        address[5], address[4], address[3], address[2], address[1], address[0],
    ];
    write[16] = reversed[2];
    write[18] = reversed[0];
    write[19] = reversed[1];
    write[20] = reversed[3];
    write[22] = reversed[4];
    write[23] = reversed[5];

    let mut reset = [0_u8; 18];
    reset[0..2].copy_from_slice(&CSR_SETREQ.to_le_bytes());
    reset[2..4].copy_from_slice(&9_u16.to_le_bytes());
    reset[6] = 0x02;
    reset[7] = 0x40;

    CsrBdAddrRewritePlan {
        write: vendor_command(&write),
        reset: vendor_command(&reset),
    }
}

pub(super) fn matches_csr_vendor_response(
    command: &CsrVendorCommand,
    event_parameters: &[u8],
) -> bool {
    let parameters = command.parameters();
    if parameters.len() < 9 || event_parameters.len() < 9 {
        return false;
    }
    let request_type = u16::from_le_bytes([parameters[1], parameters[2]]);
    matches!(request_type, CSR_GETREQ | CSR_SETREQ)
        && event_parameters[0] == CSR_BCCMD_CHANNEL
        && u16::from_le_bytes([event_parameters[1], event_parameters[2]]) == CSR_GETRESP
        && event_parameters[5..7] == parameters[5..7]
        && event_parameters[7..9] == parameters[7..9]
}

pub(super) fn parse_csr_bccmd_response(event_parameters: &[u8]) -> Result<u16, CsrCodecError> {
    if event_parameters.len() < 11 {
        return Err(CsrCodecError::MalformedResponse);
    }
    if event_parameters[0] != CSR_BCCMD_CHANNEL {
        return Err(CsrCodecError::NotCsrBccmd);
    }
    Ok(u16::from_le_bytes([
        event_parameters[9],
        event_parameters[10],
    ]))
}

#[cfg(test)]
pub(super) fn parse_csr_bd_addr_read_response(
    event_parameters: &[u8],
) -> Result<CsrBdAddrReadResponse, CsrCodecError> {
    let minimum_length = 17 + BD_ADDR_VALUE_LENGTH;
    if event_parameters.len() < minimum_length {
        return Err(CsrCodecError::MalformedResponse);
    }
    let status = parse_csr_bccmd_response(event_parameters)?;
    if status != 0 {
        return Err(CsrCodecError::FailedResponse);
    }
    let raw = &event_parameters[17..minimum_length];
    Ok(CsrBdAddrReadResponse {
        address: [raw[7], raw[6], raw[4], raw[0], raw[3], raw[2]],
        status,
    })
}

fn vendor_command(payload: &[u8]) -> CsrVendorCommand {
    let mut parameters = Vec::with_capacity(payload.len() + 1);
    parameters.push(CSR_BCCMD_CHANNEL);
    parameters.extend_from_slice(payload);
    CsrVendorCommand {
        op_code: CSR_VENDOR_OPCODE,
        parameters: parameters.into_boxed_slice(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CSR_VENDOR_OPCODE, build_csr_bd_addr_read_command, build_csr_bd_addr_volatile_rewrite_plan,
        matches_csr_vendor_response, parse_csr_bccmd_response, parse_csr_bd_addr_read_response,
    };

    const TARGET: [u8; 6] = [0x02, 0x12, 0x34, 0x56, 0x78, 0x9A];

    #[test]
    fn read_and_volatile_rewrite_match_the_pinned_csr_layout() {
        let read = build_csr_bd_addr_read_command(0x4711);
        let rewrite = build_csr_bd_addr_volatile_rewrite_plan(TARGET, 0x4711);

        assert_eq!(read.op_code(), CSR_VENDOR_OPCODE);
        assert_eq!(
            read.parameters(),
            &hex("c200000c001147037000000100040000000000000000000000")
        );
        assert_eq!(rewrite.write().op_code(), CSR_VENDOR_OPCODE);
        assert_eq!(
            rewrite.write().parameters(),
            &hex("c202000c0011470370000001000400080056009a7834001202")
        );
        assert_eq!(
            rewrite.reset().parameters(),
            &hex("c2020009000000024000000000000000000000")
        );
    }

    #[test]
    fn read_response_decodes_the_address_and_rejects_failed_status() {
        let response = parse_csr_bd_addr_read_response(&hex(
            "c201000c0011470370000001000400000056009a7834001202",
        ))
        .expect("valid CSR PSKEY_BDADDR response");

        assert_eq!(response.address(), TARGET);
        assert_eq!(response.status(), 0);

        let error = parse_csr_bd_addr_read_response(&hex(
            "c201000c0011470370341200010004000056009a7834001202",
        ))
        .expect_err("non-zero CSR status must fail");
        assert_eq!(error.to_string(), "CSR vendor response reported failure");
    }

    #[test]
    fn response_matching_uses_type_sequence_and_varid() {
        let read = build_csr_bd_addr_read_command(0x4712);
        let write = build_csr_bd_addr_volatile_rewrite_plan(TARGET, 0x4711);
        let read_response = hex("c201000c0012470370000001000400000056009a7834001202");
        let write_response = hex("c201000c0011470370000001000400080056009a7834001202");

        assert!(matches_csr_vendor_response(&read, &read_response));
        assert!(matches_csr_vendor_response(write.write(), &write_response));
        assert!(!matches_csr_vendor_response(&read, &write_response));
        assert!(!matches_csr_vendor_response(&read, &write_response[..8]));
    }

    #[test]
    fn malformed_or_non_csr_responses_fail_without_echoing_payloads() {
        let non_csr = parse_csr_bccmd_response(&hex("c100000000000000000000"))
            .expect_err("non-CSR event must fail");
        let short = parse_csr_bccmd_response(&[0xC2]).expect_err("short event must fail");

        assert_eq!(non_csr.to_string(), "vendor response is not CSR BCCMD");
        assert_eq!(short.to_string(), "CSR vendor response is malformed");
        assert!(!format!("{non_csr:?}").contains("c100"));
        assert!(!format!("{short:?}").contains("c2"));
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).expect("ASCII hex fixture");
                u8::from_str_radix(text, 16).expect("valid hex fixture")
            })
            .collect()
    }
}
