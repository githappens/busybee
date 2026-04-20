use std::io::Read;

use crate::errors::BusybeeError;
use pueue_lib::Client;
use pueue_lib::message::{LogRequest, Request, Response, TaskSelection};

/// Fetch the combined stdout+stderr log for `task_id` starting at plaintext
/// byte `offset`. Returns the new plaintext bytes and the new cursor position.
/// When the returned chunk is empty, the log hasn't grown since `offset`.
///
/// pueue-lib's `LogRequest` returns the task's output compressed with snappy's
/// framed format (`read_and_compress_log_file`). We decompress the full blob
/// here, then slice at `offset`, so callers only ever see plaintext.
///
/// v1 re-fetches and re-decompresses the full log every poll; pueue-lib 0.31
/// doesn't expose a from-offset streaming helper for raw bytes. That's fine
/// for short builds; noted as a potential optimisation target.
pub async fn fetch_log_chunk(
    client: &mut Client,
    task_id: usize,
    offset: u64,
) -> Result<(Vec<u8>, u64), BusybeeError> {
    let req = Request::Log(LogRequest {
        tasks: TaskSelection::TaskIds(vec![task_id]),
        send_logs: true,
        lines: None,
    });
    client.send_request(req).await.map_err(io)?;
    let resp = client.receive_response().await.map_err(io)?;
    match resp {
        Response::Log(m) => {
            let Some(task_log) = m.get(&task_id) else {
                return Ok((Vec::new(), offset));
            };
            let compressed = task_log.output.as_deref().unwrap_or(&[]);
            let plaintext = decompress_snappy_frames(compressed)?;
            let full_len = plaintext.len() as u64;
            if offset >= full_len {
                Ok((Vec::new(), full_len))
            } else {
                let new = plaintext[offset as usize..].to_vec();
                Ok((new, full_len))
            }
        }
        // pueued returns Failure when the log file hasn't been created yet
        // (task is queued but not yet running). Treat this as "no data yet".
        Response::Failure(_) => Ok((Vec::new(), offset)),
        other => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
    }
}

fn decompress_snappy_frames(compressed: &[u8]) -> Result<Vec<u8>, BusybeeError> {
    if compressed.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(compressed.len());
    snap::read::FrameDecoder::new(compressed)
        .read_to_end(&mut out)
        .map_err(|e| BusybeeError::Other(format!("snappy decode: {e}")))?;
    Ok(out)
}

fn io(e: impl std::fmt::Display) -> BusybeeError {
    BusybeeError::Other(format!("pueue-lib io: {e}"))
}

#[cfg(test)]
mod tests {
    use super::decompress_snappy_frames;
    use snap::write::FrameEncoder;
    use std::io::Write;

    fn encode(plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = FrameEncoder::new(&mut out);
            enc.write_all(plain).unwrap();
            enc.flush().unwrap();
        }
        out
    }

    #[test]
    fn decompress_empty_is_empty() {
        assert_eq!(decompress_snappy_frames(&[]).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn decompress_round_trips_repetitive_input() {
        let plain: Vec<u8> = b"createWriterForAudioFileFormat\n"
            .repeat(200);
        let compressed = encode(&plain);
        assert!(compressed.windows(6).any(|w| w == b"sNaPpY"));
        assert_eq!(decompress_snappy_frames(&compressed).unwrap(), plain);
    }
}
