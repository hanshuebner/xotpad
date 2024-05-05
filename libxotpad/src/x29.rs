use bytes::{Buf, BufMut, Bytes, BytesMut};

#[derive(Debug)]
pub struct X29CallUserData {
    protocol: [u8; 4],
    call_data: Vec<u8>,
}

impl X29CallUserData {
    const PAD_PROTOCOL: [u8; 4] = [0x01, 0x00, 0x00, 0x00];

    pub fn with_call_data(call_data: &[u8]) -> Result<Self, String> {
        if call_data.len() > 12 {
            return Err("call data too long".to_string());
        }

        Ok(X29CallUserData {
            protocol: X29CallUserData::PAD_PROTOCOL,
            call_data: call_data.into(),
        })
    }

    pub fn is_pad_protocol(&self) -> bool {
        self.protocol == X29CallUserData::PAD_PROTOCOL
    }

    pub fn call_data(&self) -> &[u8] {
        &self.call_data
    }

    pub fn encode(&self, buf: &mut BytesMut) -> usize {
        buf.put_slice(&self.protocol);
        buf.put_slice(&self.call_data);

        4 + self.call_data.len()
    }

    pub fn decode(mut buf: Bytes) -> Result<Self, String> {
        if buf.len() < 4 {
            return Err(format!("call user data too short: {}", buf.len()));
        }

        let mut protocol: [u8; 4] = [0; 4];

        buf.copy_to_slice(&mut protocol);

        Ok(X29CallUserData {
            protocol,
            call_data: buf.into(),
        })
    }
}

#[derive(PartialEq, Debug)]
pub enum X29PadMessage {
    Set(Vec<(u8, u8)>),
    Read(Vec<u8>),
    SetRead(Vec<(u8, u8)>),
    Indicate(Vec<(u8, u8)>),
    ClearInvitation,
}

impl X29PadMessage {
    pub fn encode(&self, buf: &mut BytesMut) -> usize {
        match self {
            X29PadMessage::Indicate(params) => {
                buf.put_u8(0x00);

                let len = encode_params(params, buf);

                1 + len
            }
            X29PadMessage::ClearInvitation => {
                buf.put_u8(0x01);

                1
            }
            _ => unimplemented!(),
        }
    }

    pub fn decode(mut buf: Bytes) -> Result<Self, String> {
        #[allow(clippy::len_zero)]
        if buf.len() < 1 {
            return Err(format!("message too short: {}", buf.len()));
        }

        let code = buf.get_u8();

        match code {
            0x02 => {
                let params = decode_params(buf)?;

                let params = params.iter().map(|p| (p.0 & 0x7f, p.1)).collect();

                Ok(X29PadMessage::Set(params))
            }
            0x04 => {
                let params = decode_params(buf)?;

                if params.iter().any(|p| p.1 != 0) {
                    return Err("invalid param for read message".into());
                }

                let params = params.iter().map(|p| p.0 & 0x7f).collect();

                Ok(X29PadMessage::Read(params))
            }
            0x06 => {
                let params = decode_params(buf)?;

                let params = params.iter().map(|p| (p.0 & 0x7f, p.1)).collect();

                Ok(X29PadMessage::SetRead(params))
            }
            0x01 => {
                #[allow(clippy::len_zero)]
                if buf.len() > 0 {
                    return Err(format!("message too long: {}", buf.len()));
                }

                Ok(X29PadMessage::ClearInvitation)
            }
            _ => Err(format!("unrecognized X.29 PAD message: {code}")),
        }
    }
}

fn encode_params(params: &[(u8, u8)], buf: &mut BytesMut) -> usize {
    let mut len = 0;

    buf.reserve(params.len() * 2);

    for &(param, value) in params {
        buf.put_u8(param);
        buf.put_u8(value);

        len += 2;
    }

    len
}

fn decode_params(mut buf: Bytes) -> Result<Vec<(u8, u8)>, String> {
    if buf.len() % 2 != 0 {
        return Err("TODO".into());
    }

    let mut params = Vec::new();

    while !buf.is_empty() {
        let (param, value) = (buf.get_u8(), buf.get_u8());

        params.push((param, value));
    }

    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_call_user_data() {
        let call_user_data = X29CallUserData::with_call_data(b"testing").unwrap();

        let mut buf = BytesMut::new();

        assert_eq!(call_user_data.encode(&mut buf), 11);

        assert_eq!(&buf[..], b"\x01\x00\x00\x00testing");
    }

    #[test]
    fn decode_call_user_data() {
        let buf = Bytes::from_static(b"\x01\x00\x00\x00testing");

        let call_user_data = X29CallUserData::decode(buf).unwrap();

        assert!(call_user_data.is_pad_protocol());
    }

    #[test]
    fn decode_set_message() {
        let buf = Bytes::from_static(b"\x02\x01\x00\x02\x7e");

        assert_eq!(
            X29PadMessage::decode(buf),
            Ok(X29PadMessage::Set(vec![(1, 0), (2, 126)]))
        );
    }

    #[test]
    fn decode_read_message() {
        let buf = Bytes::from_static(b"\x04\x01\x00\x02\x00");

        assert_eq!(
            X29PadMessage::decode(buf),
            Ok(X29PadMessage::Read(vec![1, 2]))
        );
    }

    #[test]
    fn decode_set_read_message() {
        let buf = Bytes::from_static(b"\x06\x01\x00\x02\x7e");

        assert_eq!(
            X29PadMessage::decode(buf),
            Ok(X29PadMessage::SetRead(vec![(1, 0), (2, 126)]))
        );
    }

    #[test]
    fn encode_indicate_message() {
        let message = X29PadMessage::Indicate(vec![(1, 0), (2, 126)]);

        let mut buf = BytesMut::new();

        assert_eq!(message.encode(&mut buf), 5);

        assert_eq!(&buf[..], b"\x00\x01\x00\x02\x7e");
    }

    #[test]
    fn decode_clear_invitation_message() {
        let buf = Bytes::from_static(b"\x01");

        assert_eq!(
            X29PadMessage::decode(buf),
            Ok(X29PadMessage::ClearInvitation)
        );
    }
}

#[cfg(fuzzing)]
pub mod fuzzing {
    use bytes::Bytes;

    use super::*;

    pub fn call_user_data_decode(buf: Bytes) -> Result<X29CallUserData, String> {
        X29CallUserData::decode(buf)
    }

    pub fn pad_message_decode(buf: Bytes) -> Result<X29PadMessage, String> {
        X29PadMessage::decode(buf)
    }
}
