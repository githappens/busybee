use std::io::Read;

use pueue_lib::message::{LogRequest, Request, Response, TaskSelection};
use pueue_lib::Client;

use crate::client::request;
use crate::errors::BusybeeError;

/// Fetch the combined stdout+stderr log for `task_id` from plaintext byte
/// `offset`. Returns the new bytes and the new cursor; an empty chunk means
/// the log has not grown. pueued sends the whole log snappy-framed, so it is
/// decompressed in full and sliced here.
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
    match request(client, req).await {
        Ok(Response::Log(m)) => {
            let Some(task_log) = m.get(&task_id) else {
                return Ok((Vec::new(), offset));
            };
            let compressed = task_log.output.as_deref().unwrap_or(&[]);
            let plaintext = decompress_snappy_frames(compressed)?;
            let new = plaintext.get(offset as usize..).unwrap_or(&[]).to_vec();
            Ok((new, plaintext.len() as u64))
        }
        // pueued refuses until the task has started and created its log file.
        Err(BusybeeError::EnqueueRejected(_)) => Ok((Vec::new(), offset)),
        Ok(other) => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
        Err(e) => Err(e),
    }
}

fn decompress_snappy_frames(compressed: &[u8]) -> Result<Vec<u8>, BusybeeError> {
    let mut out = Vec::with_capacity(compressed.len());
    snap::read::FrameDecoder::new(compressed)
        .read_to_end(&mut out)
        .map_err(|e| BusybeeError::Other(format!("snappy decode: {e}")))?;
    Ok(out)
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
        let plain: Vec<u8> = b"createWriterForAudioFileFormat\n".repeat(200);
        let compressed = encode(&plain);
        assert!(compressed.windows(6).any(|w| w == b"sNaPpY"));
        assert_eq!(decompress_snappy_frames(&compressed).unwrap(), plain);
    }
}
