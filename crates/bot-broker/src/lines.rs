//! Bounded line reader that rejects oversized lines without buffering past the limit.

use crate::sandbox::BoxRead;
use std::io;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Reads one newline-delimited line (excluding `\n`), returning `None` on clean EOF and an error on mid-line EOF or length overflow.
pub async fn read_line(reader: &mut BufReader<BoxRead>, max: usize) -> io::Result<Option<Vec<u8>>> {
    let too_long = || io::Error::new(io::ErrorKind::InvalidData, "line too long");
    let mut line = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        match buf.iter().position(|b| *b == b'\n') {
            Some(at) => {
                if line.len() + at > max {
                    return Err(too_long());
                }
                line.extend_from_slice(&buf[..at]);
                reader.consume(at + 1);
                return Ok(Some(line));
            }
            None => {
                if line.len() + buf.len() > max {
                    return Err(too_long());
                }
                line.extend_from_slice(buf);
                let taken = buf.len();
                reader.consume(taken);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, duplex};

    async fn reader(data: &[u8]) -> BufReader<BoxRead> {
        let (mut tx, rx) = duplex(1 << 16);
        tx.write_all(data).await.unwrap();
        drop(tx);
        BufReader::new(Box::new(rx) as BoxRead)
    }

    #[tokio::test]
    async fn reads_lines_and_reports_end_of_stream() {
        let mut r = reader(b"one\ntwo\n\nlast\n").await;
        let mut got = Vec::new();
        while let Some(line) = read_line(&mut r, 100).await.unwrap() {
            got.push(String::from_utf8(line).unwrap());
        }
        assert_eq!(got, ["one", "two", "", "last"]);
    }

    #[tokio::test]
    async fn a_line_over_the_limit_is_an_error_not_a_buffer() {
        let mut r = reader(format!("{}\nok\n", "a".repeat(50)).as_bytes()).await;
        assert!(read_line(&mut r, 49).await.is_err());
        let mut exact = reader(format!("{}\n", "a".repeat(50)).as_bytes()).await;
        assert_eq!(read_line(&mut exact, 50).await.unwrap().unwrap().len(), 50);
        let mut endless = reader(&vec![b'x'; 5000]).await;
        assert!(read_line(&mut endless, 1000).await.is_err());
    }

    #[tokio::test]
    async fn a_stream_that_ends_mid_line_is_an_error() {
        let mut r = reader(b"partial").await;
        assert!(read_line(&mut r, 100).await.is_err());
    }
}
