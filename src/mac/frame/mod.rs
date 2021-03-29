//! Partial implementation of the IEEE 802.15.4 Frame
//!
//! The main type in this module is [Frame], a type that represents an IEEE
//! 802.15.4 MAC frame. The other types in this module are supporting types
//! that are either part of [Frame] or are required to support its API.
//!
//! [Frame]: struct.Frame.html

// TODO:
// - change &mut [u8] -> bytes::BufMut
// - change &[u8] => bytes::Buf
// - remove one variant enums

use crate::mac::beacon::Beacon;
use crate::mac::command::Command;

mod frame_control;
pub mod header;
pub mod security;
mod security_control;
use aead::{
    consts::U13,
    generic_array::{ArrayLength, GenericArray},
    AeadCore, AeadInPlace, NewAead,
};
use byte::{ctx::Bytes, BytesExt, TryRead, TryWrite, LE};
use header::FrameType;
pub use header::Header;
pub use security::AuxiliarySecurityHeader;
use security_control::SecurityLevel;

use self::security::{KeyDescriptorLookup, SecurityContext, SecurityError};

/// An IEEE 802.15.4 MAC frame
///
/// Represents a MAC frame. Can be used to [decode] a frame from bytes, or
/// [encode] a frame to bytes.
///
///
/// # Decode Errors
///
/// This function returns an error, if the bytes either don't encode a valid
/// IEEE 802.15.4 frame, or encode a frame that is not fully supported by
/// this implementation. Please refer to [`DecodeError`] for details.
///
/// # Example
///
/// ``` rust
/// use ieee802154::mac::{
///     Frame,
///       Address,
///       ShortAddress,
///       FrameType,
///       FooterMode,
///       PanId,
///       Security
/// };
/// use byte::BytesExt;
///
/// # fn main() -> Result<(), ::ieee802154::mac::frame::DecodeError> {
/// // Construct a simple MAC frame. The CRC checksum (the last 2 bytes) is
/// // invalid, for the sake of convenience.
/// let bytes = [
///     0x01u8, 0x98,             // frame control
///     0x00,                   // sequence number
///     0x12, 0x34, 0x56, 0x78, // PAN identifier and address of destination
///     0x12, 0x34, 0x9a, 0xbc, // PAN identifier and address of source
///     0xde, 0xf0,             // payload
///     0x12, 0x34,             // payload
/// ];
///
/// let frame: Frame = bytes.read_with(&mut 0, FooterMode::Explicit).unwrap();
/// let header = frame.header;
///
/// assert_eq!(frame.header.seq,       0x00);
/// assert_eq!(header.frame_type,      FrameType::Data);
/// assert_eq!(header.security,        false);
/// assert_eq!(header.frame_pending,   false);
/// assert_eq!(header.ack_request,     false);
/// assert_eq!(header.pan_id_compress, false);
///
/// assert_eq!(
///     frame.header.destination,
///     Some(Address::Short(PanId(0x3412), ShortAddress(0x7856)))
/// );
/// assert_eq!(
///     frame.header.source,
///     Some(Address::Short(PanId(0x3412), ShortAddress(0xbc9a)))
/// );
///
/// assert_eq!(frame.payload, &[0xde, 0xf0]);
///
/// assert_eq!(frame.footer, [0x12, 0x34]);
/// #
/// # Ok(())
/// # }
/// ```
/// Encodes the frame into a buffer
///
/// # Example
///
/// ## allocation allowed
/// ``` rust
/// use ieee802154::mac::{
///   Frame,
///   FrameContent,
///   FooterMode,
///   Address,
///   ShortAddress,
///   FrameType,
///   FrameVersion,
///   Header,
///   PanId,
///   Security,
/// };
/// use byte::BytesExt;
///
/// let frame = Frame {
///     header: Header {
///         frame_type:      FrameType::Data,
///         security:        false,
///         frame_pending:   false,
///         ack_request:     false,
///         pan_id_compress: false,
///         version:         FrameVersion::Ieee802154_2006,
///
///         seq:             0x00,
///         destination: Some(Address::Short(PanId(0x1234), ShortAddress(0x5678))),
///         source:      Some(Address::Short(PanId(0x1234), ShortAddress(0x9abc))),
///     },
///     content: FrameContent::Data,
///     payload: &[0xde, 0xf0],
///     footer:  [0x12, 0x34]
/// };
///
/// // Work also with `let mut bytes = Vec::new()`;
/// let mut bytes = [0u8; 32];
/// let mut len = 0usize;
///
/// bytes.write_with(&mut len, frame, FooterMode::Explicit).unwrap();
///
/// let expected_bytes = [
///     0x01, 0x98,             // frame control
///     0x00,                   // sequence number
///     0x34, 0x12, 0x78, 0x56, // PAN identifier and address of destination
///     0x34, 0x12, 0xbc, 0x9a, // PAN identifier and address of source
///     0xde, 0xf0,             // payload
///     0x12, 0x34              // footer
/// ];
/// assert_eq!(bytes[..len], expected_bytes);
/// ```
///
/// [decode]: #method.try_read
/// [encode]: #method.try_write
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Frame<'p> {
    /// Header
    pub header: Header,

    /// Content
    pub content: FrameContent,

    /// Payload
    pub payload: &'p [u8],

    /// Footer
    ///
    /// This is a 2-byte CRC checksum.
    ///
    /// When creating an instance of this struct for encoding, you don't
    /// necessarily need to write an actual CRC checksum here. [`Frame::encode`]
    /// can omit writing this checksum, for example if the transceiver hardware
    /// automatically adds the checksum for you.
    pub footer: [u8; 2],
}

/// A context that is used for serializing and deserializing frames, which also
/// stores the frame counter
pub struct FrameSerDesContext<'a, AEAD, KEYDESCLO>
where
    AEAD: NewAead + AeadInPlace,
    AEAD::NonceSize: ArrayLength<U13>,
    KEYDESCLO: KeyDescriptorLookup,
{
    /// The footer mode to use when handling frames
    pub footer_mode: FooterMode,
    /// The security context for handling frames (if any)
    pub security_ctx: Option<SecurityContext<'a, AEAD, KEYDESCLO>>,
}

impl Frame<'_> {
    fn secure_frame<AEAD, KEYDESCLO, NONCEGEN>(
        &mut self,
        context: &mut SecurityContext<AEAD, KEYDESCLO>,
    ) -> Result<(), SecurityError>
    where
        AEAD: NewAead + AeadInPlace,
        AEAD::NonceSize: ArrayLength<U13>,
        KEYDESCLO: KeyDescriptorLookup,
    {
        let frame_counter = &mut context.frame_counter;
        if self.header.security {
            // Procedure 7.2.1
            if let Some(aux_sec_header) = self.header.auxiliary_security_header {
                let auth_len = match aux_sec_header.control.security_level {
                    SecurityLevel::None => 0,
                    SecurityLevel::MIC32 => 4,
                    SecurityLevel::MIC64 => 8,
                    SecurityLevel::MIC128 => 16,
                    SecurityLevel::ENC => 0,
                    SecurityLevel::ENCMIC32 => 4,
                    SecurityLevel::ENCMIC64 => 8,
                    SecurityLevel::ENCMIC128 => 16,
                };
                let aux_len = aux_sec_header.get_octet_size();

                // If AuthLen plus AuxLen plus FCS is bigger than aMaxPHYPacketSize
                // 7.2.1 b4
                if auth_len + aux_len + 2 > 127 {
                    return Err(SecurityError::FrameTooLong)?;
                }

                if aux_sec_header.control.security_level == SecurityLevel::None {}

                if *frame_counter == 0xFFFFFFFF {
                    return Err(SecurityError::CounterError)?;
                }

                if let Some(key) = context.key_provider.lookup_key(
                    security::KeyAddressMode::DstAddrMode,
                    aux_sec_header.key_identifier,
                    self.header.destination,
                ) {
                    match aux_sec_header.control.security_level {
                        SecurityLevel::None => {}
                        SecurityLevel::MIC32 | SecurityLevel::MIC64 | SecurityLevel::MIC128 => {
                            let aead_in_place = match AEAD::new_from_slice(&key.key) {
                                Ok(key) => key,
                                Err(_) => return Err(SecurityError::KeyFailure)?,
                            };
                            let nonce = GenericArray::default();
                            let tag = aead_in_place.encrypt_in_place_detached(
                                &nonce,
                                &self.payload,
                                &mut [],
                            );
                        }
                        SecurityLevel::ENC => {}
                        SecurityLevel::ENCMIC32 => {}
                        SecurityLevel::ENCMIC64 => {}
                        SecurityLevel::ENCMIC128 => {}
                    }
                } else {
                    return Err(SecurityError::UnavailableKey)?;
                }
            } else {
                panic!("Security on but AuxSecHeader absent")
            }
        } else {
            // Not a fan of the fact that we can't pass some actually
            // useful information to the layer above this, only byte::Result
            if self.header.auxiliary_security_header.is_some() {
                panic!("Security off but AuxSecHeader present")
            }
        }
        Ok(())
    }
}

impl<AEAD, KEYDESCLO> TryWrite<FrameSerDesContext<'_, AEAD, KEYDESCLO>> for Frame<'_>
where
    AEAD: NewAead + AeadInPlace,
    AEAD::NonceSize: ArrayLength<U13>,
    KEYDESCLO: KeyDescriptorLookup,
{
    fn try_write(
        self,
        bytes: &mut [u8],
        context: FrameSerDesContext<AEAD, KEYDESCLO>,
    ) -> byte::Result<usize> {
        let mode = context.footer_mode;
        let offset = &mut 0;

        bytes.write(offset, self.header)?;
        bytes.write(offset, self.content)?;

        bytes.write(offset, self.payload)?;
        match mode {
            FooterMode::None => {}
            FooterMode::Explicit => bytes.write(offset, &self.footer[..])?,
        }
        Ok(*offset)
    }
}

impl<'a, AEAD> TryRead<'a, (FooterMode, AEAD)> for Frame<'a>
where
    AEAD: AeadInPlace,
{
    fn try_read(bytes: &'a [u8], context: (FooterMode, AEAD)) -> byte::Result<(Self, usize)> {
        let (mode, _aead) = context;

        let offset = &mut 0;
        let header = bytes.read(offset)?;
        let content = bytes.read_with(offset, &header)?;
        let (payload, footer) = match mode {
            FooterMode::None => (
                bytes.read_with(offset, Bytes::Len(bytes.len() - *offset))?,
                0u16,
            ),
            FooterMode::Explicit => (
                bytes.read_with(offset, Bytes::Len(bytes.len() - *offset - 2))?,
                bytes.read_with(offset, LE)?,
            ),
        };

        Ok((
            Frame {
                header: header,
                content: content,
                payload,
                footer: footer.to_le_bytes(),
            },
            *offset,
        ))
    }
}

///
/// Controls whether the footer is read/written with the frame
///
/// Eventually, this should support three options:
/// 1. Don't read or write the footer
/// 2. Calculate the 2-byte CRC checksum and write that as the footer or check against read value
/// 3. Read into or write the footer from the `footer` field
///
/// For now, only 1 and 3 are supported.
///
/// [`Frame::try_write`](Frame::try_write)
pub enum FooterMode {
    /// Don't read/write the footer
    None,
    /// Read into or write the footer from the `footer` field
    Explicit,
}

impl Default for FooterMode {
    fn default() -> Self {
        Self::None
    }
}

/// Content of a frame
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FrameContent {
    /// Beacon frame content
    Beacon(Beacon),
    /// Data frame
    Data,
    /// Acknowledgement frame
    Acknowledgement,
    /// MAC command frame
    Command(Command),
}

impl TryWrite for FrameContent {
    fn try_write(self, bytes: &mut [u8], _ctx: ()) -> byte::Result<usize> {
        let offset = &mut 0;
        match self {
            FrameContent::Beacon(beacon) => bytes.write(offset, beacon)?,
            FrameContent::Data | FrameContent::Acknowledgement => (),
            FrameContent::Command(command) => bytes.write(offset, command)?,
        };
        Ok(*offset)
    }
}

impl TryRead<'_, &Header> for FrameContent {
    fn try_read(bytes: &[u8], header: &Header) -> byte::Result<(Self, usize)> {
        let offset = &mut 0;
        Ok((
            match header.frame_type {
                FrameType::Beacon => FrameContent::Beacon(bytes.read(offset)?),
                FrameType::Data => FrameContent::Data,
                FrameType::Acknowledgement => FrameContent::Acknowledgement,
                FrameType::MacCommand => FrameContent::Command(bytes.read(offset)?),
            },
            *offset,
        ))
    }
}

/// Signals an error that occured while decoding bytes
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DecodeError {
    /// Buffer does not contain enough bytes
    NotEnoughBytes,

    /// The frame type is invalid
    InvalidFrameType(u8),

    /// The frame has the security bit set, which is not supported
    SecurityNotSupported,

    /// The frame's address mode is invalid
    InvalidAddressMode(u8),

    /// The frame's version is invalid or not supported
    InvalidFrameVersion(u8),

    /// The auxiliary security header's security level is invalid
    InvalidSecurityLevel(u8),

    /// The auxiliary security header's key identifier mode is invalid
    InvalidKeyIdentifierMode(u8),

    /// Security is disabled, but an Auxiliary Security Header is set
    SecurityNotEnabled,

    /// Security is enabled, but no Auxiliary Security Header is present
    AuxSecHeaderAbsent,

    /// The data stream contains an invalid value
    InvalidValue,
}

impl From<DecodeError> for byte::Error {
    fn from(e: DecodeError) -> Self {
        match e {
            DecodeError::NotEnoughBytes => byte::Error::Incomplete,
            DecodeError::InvalidFrameType(_) => byte::Error::BadInput {
                err: "InvalidFrameType",
            },
            DecodeError::SecurityNotSupported => byte::Error::BadInput {
                err: "SecurityNotSupported",
            },
            DecodeError::InvalidAddressMode(_) => byte::Error::BadInput {
                err: "InvalidAddressMode",
            },
            DecodeError::InvalidFrameVersion(_) => byte::Error::BadInput {
                err: "InvalidFrameVersion",
            },
            DecodeError::InvalidValue => byte::Error::BadInput {
                err: "InvalidValue",
            },
            DecodeError::InvalidSecurityLevel(_) => byte::Error::BadInput {
                err: "InvalidSecurityLevel",
            },
            DecodeError::InvalidKeyIdentifierMode(_) => byte::Error::BadInput {
                err: "InvalidKeyIdentifierMode",
            },
            DecodeError::SecurityNotEnabled => byte::Error::BadInput {
                err: "SecurityNotEnabled",
            },
            DecodeError::AuxSecHeaderAbsent => byte::Error::BadInput {
                err: "AuxSecHeaderAbsent",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac::beacon;
    use crate::mac::command;
    use crate::mac::{Address, ExtendedAddress, FrameVersion, PanId, ShortAddress};

    #[test]
    fn decode_ver0_pan_id_compression() {
        let data = [
            0x41, 0x88, 0x91, 0x8f, 0x20, 0xff, 0xff, 0x33, 0x44, 0x00, 0x00,
        ];
        let frame: Frame = data.read(&mut 0).unwrap();
        let hdr = frame.header;
        assert_eq!(hdr.frame_type, FrameType::Data);
        assert_eq!(hdr.security, false);
        assert_eq!(hdr.frame_pending, false);
        assert_eq!(hdr.ack_request, false);
        assert_eq!(hdr.pan_id_compress, true);
        assert_eq!(hdr.version, FrameVersion::Ieee802154_2003);
        assert_eq!(
            frame.header.destination,
            Some(Address::Short(PanId(0x208f), ShortAddress(0xffff)))
        );
        assert_eq!(
            frame.header.source,
            Some(Address::Short(PanId(0x208f), ShortAddress(0x4433)))
        );
        assert_eq!(frame.header.seq, 145);
    }

    #[test]
    fn decode_ver0_pan_id_compression_bad() {
        let data = [
            0x41, 0x80, 0x91, 0x8f, 0x20, 0xff, 0xff, 0x33, 0x44, 0x00, 0x00,
        ];
        let frame = data.read::<Frame>(&mut 0);
        assert!(frame.is_err());
        if let Err(e) = frame {
            assert_eq!(e, DecodeError::InvalidAddressMode(0).into())
        }
    }

    #[test]
    fn decode_ver0_extended() {
        let data = [
            0x21, 0xc8, 0x8b, 0xff, 0xff, 0x02, 0x00, 0x23, 0x00, 0x60, 0xe2, 0x16, 0x21, 0x1c,
            0x4a, 0xc2, 0xae, 0xaa, 0xbb, 0xcc,
        ];
        let frame: Frame = data.read(&mut 0).unwrap();
        let hdr = frame.header;
        assert_eq!(hdr.frame_type, FrameType::Data);
        assert_eq!(hdr.security, false);
        assert_eq!(hdr.frame_pending, false);
        assert_eq!(hdr.ack_request, true);
        assert_eq!(hdr.pan_id_compress, false);
        assert_eq!(hdr.version, FrameVersion::Ieee802154_2003);
        assert_eq!(
            frame.header.destination,
            Some(Address::Short(PanId(0xffff), ShortAddress(0x0002)))
        );
        assert_eq!(
            frame.header.source,
            Some(Address::Extended(
                PanId(0x0023),
                ExtendedAddress(0xaec24a1c2116e260)
            ))
        );
        assert_eq!(frame.header.seq, 139);
    }

    #[test]
    fn encode_ver0_short() {
        let frame = Frame {
            header: Header {
                frame_type: FrameType::Data,
                security: false,
                frame_pending: false,
                ack_request: false,
                pan_id_compress: false,
                version: FrameVersion::Ieee802154_2003,
                destination: Some(Address::Short(PanId(0x1234), ShortAddress(0x5678))),
                source: Some(Address::Short(PanId(0x4321), ShortAddress(0x9abc))),
                seq: 0x01,
                auxiliary_security_header: None,
            },
            content: FrameContent::Data,
            payload: &[0xde, 0xf0],
            footer: [0x00, 0x00],
        };
        let mut buf = [0u8; 32];
        let mut len = 0usize;
        buf.write(&mut len, frame).unwrap();
        assert_eq!(len, 13);
        assert_eq!(
            buf[..len],
            [0x01, 0x88, 0x01, 0x34, 0x12, 0x78, 0x56, 0x21, 0x43, 0xbc, 0x9a, 0xde, 0xf0]
        );
    }

    #[test]
    fn encode_ver1_extended() {
        let frame = Frame {
            header: Header {
                frame_type: FrameType::Beacon,
                security: false,
                frame_pending: true,
                ack_request: false,
                pan_id_compress: false,
                version: FrameVersion::Ieee802154_2006,
                destination: Some(Address::Extended(
                    PanId(0x1234),
                    ExtendedAddress(0x1122334455667788),
                )),
                source: Some(Address::Short(PanId(0x4321), ShortAddress(0x9abc))),
                seq: 0xff,
                auxiliary_security_header: None,
            },
            content: FrameContent::Beacon(beacon::Beacon {
                superframe_spec: beacon::SuperframeSpecification {
                    beacon_order: beacon::BeaconOrder::OnDemand,
                    superframe_order: beacon::SuperframeOrder::Inactive,
                    final_cap_slot: 15,
                    battery_life_extension: false,
                    pan_coordinator: false,
                    association_permit: false,
                },
                guaranteed_time_slot_info: beacon::GuaranteedTimeSlotInformation::new(),
                pending_address: beacon::PendingAddress::new(),
            }),
            payload: &[0xde, 0xf0],
            footer: [0x00, 0x00],
        };
        let mut buf = [0u8; 32];
        let mut len = 0usize;
        buf.write(&mut len, frame).unwrap();
        assert_eq!(len, 23);
        assert_eq!(
            buf[..len],
            [
                0x10, 0x9c, 0xff, 0x34, 0x12, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x21,
                0x43, 0xbc, 0x9a, 0xff, 0x0f, 0x00, 0x00, 0xde, 0xf0
            ]
        );
    }

    #[test]
    fn encode_ver0_pan_compress() {
        let frame = Frame {
            header: Header {
                frame_type: FrameType::Acknowledgement,
                security: false,
                frame_pending: false,
                ack_request: false,
                pan_id_compress: true,
                version: FrameVersion::Ieee802154_2003,
                destination: Some(Address::Extended(
                    PanId(0x1234),
                    ExtendedAddress(0x1122334455667788),
                )),
                source: Some(Address::Short(PanId(0x1234), ShortAddress(0x9abc))),
                seq: 0xff,
                auxiliary_security_header: None,
            },
            content: FrameContent::Acknowledgement,
            payload: &[],
            footer: [0x00, 0x00],
        };
        let mut buf = [0u8; 32];
        let mut len = 0usize;
        buf.write(&mut len, frame).unwrap();
        assert_eq!(len, 15);
        assert_eq!(
            buf[..len],
            [
                0x42, 0x8c, 0xff, 0x34, 0x12, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0xbc,
                0x9a
            ]
        );
    }

    #[test]
    fn encode_ver2_none() {
        let frame = Frame {
            header: Header {
                frame_type: FrameType::MacCommand,
                security: false,
                frame_pending: false,
                ack_request: true,
                pan_id_compress: false,
                version: FrameVersion::Ieee802154,
                destination: None,
                source: Some(Address::Short(PanId(0x1234), ShortAddress(0x9abc))),
                seq: 0xff,
                auxiliary_security_header: None,
            },
            content: FrameContent::Command(command::Command::DataRequest),
            payload: &[],
            footer: [0x00, 0x00],
        };
        let mut buf = [0u8; 32];
        let mut len = 0usize;
        buf.write(&mut len, frame).unwrap();
        assert_eq!(len, 8);
        assert_eq!(buf[..len], [0x23, 0xa0, 0xff, 0x34, 0x12, 0xbc, 0x9a, 0x04]);
    }
}
