use std::convert::TryInto;

const MAGIC: &[u8; 4] = b"ECB1";
const HEADER_LEN: usize = MAGIC.len() + 1;

#[derive(Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Hello,
    Config(&'a [u8]),
    Ping(u64),
    Pong(u64),
    Audio(&'a [u8]),
}

impl<'a> Packet<'a> {
    const HELLO: u8 = 1;
    const CONFIG: u8 = 2;
    const PING: u8 = 3;
    const PONG: u8 = 4;
    const AUDIO: u8 = 5;

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Packet::Hello => Self::encode_packet(Self::HELLO, &[], out),
            Packet::Config(payload) => Self::encode_packet(Self::CONFIG, payload, out),
            Packet::Ping(id) => Self::encode_packet(Self::PING, &id.to_be_bytes(), out),
            Packet::Pong(id) => Self::encode_packet(Self::PONG, &id.to_be_bytes(), out),
            Packet::Audio(payload) => Self::encode_packet(Self::AUDIO, payload, out),
        }
    }

    fn encode_packet(kind: u8, payload: &[u8], out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(MAGIC);
        out.push(kind);
        out.extend_from_slice(payload);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PacketParseError {
    Invalid,
    TooShort,
}

impl<'a> TryFrom<&'a [u8]> for Packet<'a> {
    type Error = PacketParseError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() < HEADER_LEN {
            return Err(PacketParseError::TooShort);
        }
        if &data[..MAGIC.len()] != MAGIC {
            return Err(PacketParseError::Invalid);
        }

        let payload = &data[HEADER_LEN..];
        match data[MAGIC.len()] {
            Packet::HELLO if payload.is_empty() => Ok(Packet::Hello),
            Packet::CONFIG => Ok(Packet::Config(payload)),
            Packet::PING => parse_u64(payload).map(Packet::Ping),
            Packet::PONG => parse_u64(payload).map(Packet::Pong),
            Packet::AUDIO => Ok(Packet::Audio(payload)),
            _ => Err(PacketParseError::Invalid),
        }
    }
}

fn parse_u64(payload: &[u8]) -> Result<u64, PacketParseError> {
    let bytes: [u8; 8] = payload.try_into().map_err(|_| PacketParseError::Invalid)?;
    Ok(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_parses_ping() {
        let mut out = Vec::new();
        Packet::Ping(42).encode(&mut out);
        assert_eq!(Packet::try_from(out.as_slice()), Ok(Packet::Ping(42)));
    }

    #[test]
    fn encodes_and_parses_audio() {
        let mut out = Vec::new();
        Packet::Audio(&[1, 2, 3]).encode(&mut out);
        assert_eq!(
            Packet::try_from(out.as_slice()),
            Ok(Packet::Audio(&[1, 2, 3][..]))
        );
    }
}
