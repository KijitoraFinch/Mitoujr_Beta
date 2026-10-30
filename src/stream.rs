//! Whitespace-separated JSON values; output is compact JSON Lines.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;
use std::io::{BufReader, Read};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub struct Input(mpsc::Receiver<Result<Value>>);

impl Input {
    pub fn open(path: &Path) -> Self {
        let path = path.to_owned();
        let (tx, rx) = mpsc::channel(1);
        // serde's streaming parser handles pretty JSON and large values without
        // repeatedly parsing incomplete buffers. Backpressure bounds read-ahead.
        tokio::task::spawn_blocking(move || {
            let read = || -> Result<()> {
                let reader: Box<dyn Read> = if path.as_os_str() == "-" {
                    Box::new(std::io::stdin().lock())
                } else {
                    Box::new(
                        std::fs::File::open(&path)
                            .with_context(|| format!("read {}", path.display()))?,
                    )
                };
                for (index, value) in serde_json::Deserializer::from_reader(BufReader::new(reader))
                    .into_iter::<Value>()
                    .enumerate()
                {
                    let value = value
                        .with_context(|| format!("invalid JSON input at record {}", index + 1))?;
                    if tx.blocking_send(Ok(value)).is_err() {
                        break;
                    }
                }
                Ok(())
            };
            if let Err(error) = read() {
                let _ = tx.blocking_send(Err(error));
            }
        });
        Self(rx)
    }

    pub async fn next(&mut self) -> Result<Option<Value>> {
        self.0.recv().await.transpose()
    }

    pub async fn one(&mut self) -> Result<Value> {
        let value = self
            .next()
            .await?
            .context("expected one JSON value, got empty input")?;
        ensure!(
            self.next().await?.is_none(),
            "expected exactly one JSON value"
        );
        Ok(value)
    }
}

pub async fn emit(value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut output = tokio::io::stdout();
    output
        .write_all(&bytes)
        .await
        .context("write JSON output")?;
    output.flush().await.context("flush JSON output")
}
