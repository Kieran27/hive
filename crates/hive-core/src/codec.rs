//! `[u32 LE length][MessagePack payload]` framing over any async stream.

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame accepted; a scrollback replay is at most ~1 MB.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    let payload = rmp_serde::to_vec(msg)?;
    if payload.len() > MAX_FRAME_LEN {
        bail!("frame too large: {} bytes", payload.len());
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let buf = encode(msg)?;
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// `Ok(None)` on a clean EOF between frames.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        bail!("frame too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.context("truncated frame")?;
    Ok(Some(rmp_serde::from_slice(&buf).context("bad frame")?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientRequest, ServerEvent};

    #[tokio::test]
    async fn round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        let req = ClientRequest::Input {
            session: "s1".into(),
            data: b"\x1b[A".to_vec(),
        };
        write_frame(&mut a, &req).await.unwrap();
        let ev = ServerEvent::Output {
            session: "s1".into(),
            seq: 42,
            data: vec![0u8; 70_000],
        };
        write_frame(&mut a, &ev).await.unwrap();
        drop(a);
        match read_frame::<_, ClientRequest>(&mut b)
            .await
            .unwrap()
            .unwrap()
        {
            ClientRequest::Input { session, data } => {
                assert_eq!(session, "s1");
                assert_eq!(data, b"\x1b[A");
            }
            other => panic!("unexpected {other:?}"),
        }
        match read_frame::<_, ServerEvent>(&mut b).await.unwrap().unwrap() {
            ServerEvent::Output { seq, data, .. } => {
                assert_eq!(seq, 42);
                assert_eq!(data.len(), 70_000);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(read_frame::<_, ServerEvent>(&mut b)
            .await
            .unwrap()
            .is_none());
    }
}
